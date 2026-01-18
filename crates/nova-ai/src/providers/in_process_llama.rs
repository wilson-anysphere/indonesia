#![cfg(feature = "local-llm")]

use crate::{
    providers::LlmProvider,
    types::{AiStream, ChatMessage, ChatRequest, ChatRole},
    AiError,
};
use async_stream::try_stream;
use async_trait::async_trait;
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::{BatchAddError, LlamaBatch},
    model::{AddBos, LlamaChatMessage, LlamaModel, Special},
    sampling::LlamaSampler,
    ChatTemplateError, DecodeError, LlamaCppError,
};
use nova_config::{AiProviderConfig, InProcessLlamaConfig};
use std::{
    num::NonZeroU32,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};
use tokio_util::sync::CancellationToken;

const MAX_CONTEXT_SIZE_TOKENS: usize = 8_192;

static IN_PROCESS_LLAMA_MUTEX_POISONED_LOGGED: OnceLock<()> = OnceLock::new();

#[derive(Clone)]
pub struct InProcessLlamaProvider {
    model_path: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    backend: LlamaBackend,
    model: LlamaModel,
    cfg: InProcessLlamaConfig,
}

impl InProcessLlamaProvider {
    pub fn new(cfg: &AiProviderConfig) -> Result<Self, AiError> {
        let in_process = cfg.in_process_llama.as_ref().ok_or_else(|| {
            AiError::InvalidConfig(
                "ai.provider.in_process_llama must be set when kind = \"in_process_llama\"".into(),
            )
        })?;

        validate_in_process_config(in_process)?;

        if !in_process.model_path.is_file() {
            return Err(AiError::InvalidConfig(format!(
                "GGUF model file not found: {}",
                in_process.model_path.display()
            )));
        }

        // llama.cpp backend init is effectively global; it can only be initialized once.
        // llama-cpp-2 represents this as an error that can be safely ignored.
        let backend = match LlamaBackend::init() {
            Ok(backend) => backend,
            Err(LlamaCppError::BackendAlreadyInitialized) => LlamaBackend {},
            Err(err) => {
                return Err(AiError::InvalidConfig(format!(
                    "failed to initialize llama.cpp backend: {err}"
                )));
            }
        };

        let model_params = llama_cpp_2::model::params::LlamaModelParams::default()
            .with_n_gpu_layers(in_process.gpu_layers);

        let model = LlamaModel::load_from_file(&backend, &in_process.model_path, &model_params)
            .map_err(|err| {
                AiError::InvalidConfig(format!(
                    "failed to load GGUF model {}: {err}",
                    in_process.model_path.display()
                ))
            })?;

        Ok(Self {
            model_path: in_process.model_path.clone(),
            inner: Arc::new(Mutex::new(Inner {
                backend,
                model,
                cfg: in_process.clone(),
            })),
        })
    }

    fn model_id(&self) -> String {
        self.model_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.model_path.display().to_string())
    }
}

#[async_trait]
impl LlmProvider for InProcessLlamaProvider {
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<String, AiError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = match inner.lock() {
                Ok(guard) => guard,
                Err(err) => {
                    if IN_PROCESS_LLAMA_MUTEX_POISONED_LOGGED.set(()).is_ok() {
                        tracing::error!(
                            target = "nova.ai",
                            "in-process llama provider mutex poisoned; continuing with inner value"
                        );
                    }
                    err.into_inner()
                }
            };
            guard.complete(request, cancel, None)
        })
        .await
        .map_err(|err| AiError::UnexpectedResponse(format!("llama task join error: {err}")))?
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        let inner = self.inner.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, AiError>>();

        let _handle = tokio::task::spawn_blocking(move || {
            let mut guard = match inner.lock() {
                Ok(guard) => guard,
                Err(err) => {
                    if IN_PROCESS_LLAMA_MUTEX_POISONED_LOGGED.set(()).is_ok() {
                        tracing::error!(
                            target = "nova.ai",
                            "in-process llama provider mutex poisoned; continuing with inner value"
                        );
                    }
                    err.into_inner()
                }
            };
            let result = guard.complete(request, cancel, Some(&tx));
            if let Err(err) = result {
                let _ = tx.send(Err(err));
            }
            // Drop the sender to close the stream.
        });

        let stream = try_stream! {
            while let Some(item) = rx.recv().await {
                yield item?;
            }
        };

        Ok(Box::pin(stream))
    }

    async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        Ok(vec![self.model_id()])
    }
}

fn validate_in_process_config(cfg: &InProcessLlamaConfig) -> Result<(), AiError> {
    if cfg.context_size == 0 {
        return Err(AiError::InvalidConfig(
            "ai.provider.in_process_llama.context_size must be >= 1".into(),
        ));
    }
    if cfg.context_size > MAX_CONTEXT_SIZE_TOKENS {
        return Err(AiError::InvalidConfig(format!(
            "ai.provider.in_process_llama.context_size must be <= {MAX_CONTEXT_SIZE_TOKENS}"
        )));
    }
    if cfg.temperature.is_nan() || cfg.temperature < 0.0 {
        return Err(AiError::InvalidConfig(
            "ai.provider.in_process_llama.temperature must be >= 0".into(),
        ));
    }
    if !(0.0..=1.0).contains(&cfg.top_p) {
        return Err(AiError::InvalidConfig(
            "ai.provider.in_process_llama.top_p must be within [0, 1]".into(),
        ));
    }
    Ok(())
}

impl Inner {
    fn complete(
        &mut self,
        request: ChatRequest,
        cancel: CancellationToken,
        stream: Option<&tokio::sync::mpsc::UnboundedSender<Result<String, AiError>>>,
    ) -> Result<String, AiError> {
        static AVAILABLE_PARALLELISM_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

        let max_new_tokens = request
            .max_tokens
            .ok_or_else(|| AiError::InvalidConfig("missing max_tokens".into()))?;

        let prompt = build_prompt(&self.model, request)?;

        let mut ctx_params = LlamaContextParams::default().with_n_ctx(Some(
            NonZeroU32::new(self.cfg.context_size as u32).ok_or_else(|| {
                AiError::InvalidConfig(
                    "ai.provider.in_process_llama.context_size must fit in u32".into(),
                )
            })?,
        ));

        if let Some(threads) = self
            .cfg
            .threads
            .and_then(|t| if t == 0 { None } else { Some(t) })
            .or_else(|| match std::thread::available_parallelism() {
                Ok(n) => Some(n.get()),
                Err(err) => {
                    if AVAILABLE_PARALLELISM_ERROR_LOGGED.set(()).is_ok() {
                        tracing::debug!(
                            target = "nova.ai",
                            error = %err,
                            "failed to query available parallelism; using default llama thread settings"
                        );
                    }
                    None
                }
            })
        {
            let threads = i32::try_from(threads.min(i32::MAX as usize)).unwrap_or(i32::MAX);
            ctx_params = ctx_params
                .with_n_threads(threads)
                .with_n_threads_batch(threads);
        }

        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .map_err(|err| {
                AiError::UnexpectedResponse(format!("failed to create context: {err}"))
            })?;

        let tokens = self
            .model
            .str_to_token(&prompt, AddBos::Always)
            .map_err(|err| {
                AiError::UnexpectedResponse(format!("failed to tokenize prompt: {err}"))
            })?;

        let n_ctx = ctx.n_ctx() as usize;
        let total_tokens_needed = tokens.len().saturating_add(max_new_tokens as usize);
        if total_tokens_needed > n_ctx {
            return Err(AiError::InvalidConfig(format!(
                "prompt + max_tokens ({total_tokens_needed}) exceeds context window ({n_ctx}); reduce max_tokens or increase context_size"
            )));
        }

        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
        batch
            .add_sequence(&tokens, 0, false)
            .map_err(map_batch_add_error)?;

        ctx.decode(&mut batch).map_err(map_decode_error)?;

        let mut sampler = LlamaSampler::chain_simple([
            LlamaSampler::temp(self.cfg.temperature),
            LlamaSampler::top_p(self.cfg.top_p, 1),
            LlamaSampler::dist(1234),
        ]);

        // Generate up to `max_new_tokens`.
        let mut generated_bytes = Vec::with_capacity(max_new_tokens as usize * 4);
        let mut utf8_pending: Vec<u8> = Vec::new();

        let mut n_cur = batch.n_tokens();
        let n_target = i32::try_from(tokens.len() + max_new_tokens as usize)
            .unwrap_or(i32::MAX)
            .min(i32::try_from(n_ctx).unwrap_or(i32::MAX));

        while n_cur < n_target {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }

            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);

            if self.model.is_eog_token(token) {
                break;
            }

            let bytes = self
                .model
                .token_to_bytes(token, Special::Tokenize)
                .map_err(|err| {
                    AiError::UnexpectedResponse(format!("token decode failed: {err}"))
                })?;

            if let Some(stream) = stream {
                utf8_pending.extend_from_slice(&bytes);
                flush_utf8_stream(stream, &mut utf8_pending);
            } else {
                generated_bytes.extend_from_slice(&bytes);
            }

            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .map_err(map_batch_add_error)?;

            n_cur += 1;
            ctx.decode(&mut batch).map_err(map_decode_error)?;
        }

        if let Some(stream) = stream {
            // Flush any remaining bytes (lossy if needed).
            if !utf8_pending.is_empty() {
                let out = String::from_utf8_lossy(&utf8_pending).to_string();
                let _ = stream.send(Ok(out));
            }
            Ok(String::new())
        } else {
            Ok(String::from_utf8_lossy(&generated_bytes).to_string())
        }
    }
}

fn flush_utf8_stream(
    stream: &tokio::sync::mpsc::UnboundedSender<Result<String, AiError>>,
    pending: &mut Vec<u8>,
) {
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                if !text.is_empty() {
                    let _ = stream.send(Ok(text.to_string()));
                }
                pending.clear();
                return;
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if valid == 0 {
                    // If this is a hard UTF-8 error, drop a byte to avoid spinning forever.
                    if err.error_len().is_some() && !pending.is_empty() {
                        pending.remove(0);
                    }
                    return;
                }

                // SAFETY: `valid_up_to` guarantees this prefix is valid UTF-8.
                let prefix = unsafe { std::str::from_utf8_unchecked(&pending[..valid]) };
                if !prefix.is_empty() {
                    let _ = stream.send(Ok(prefix.to_string()));
                }
                pending.drain(..valid);
            }
        }
    }
}

fn build_prompt(model: &LlamaModel, request: ChatRequest) -> Result<String, AiError> {
    if request.messages.is_empty() {
        return Ok(String::new());
    }

    let mut messages = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let role = match message.role {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        };

        messages.push(
            LlamaChatMessage::new(role.to_string(), message.content.clone()).map_err(|err| {
                AiError::UnexpectedResponse(format!("failed to build chat message: {err}"))
            })?,
        );
    }

    // Prefer the model's baked-in template (this is the llama.cpp recommended path).
    match model.chat_template(None) {
        Ok(template) => model
            .apply_chat_template(&template, &messages, true)
            .map_err(|err| AiError::UnexpectedResponse(format!("chat template failed: {err}"))),
        Err(ChatTemplateError::MissingTemplate) => Ok(fallback_prompt(&request.messages)),
        Err(err) => Err(AiError::UnexpectedResponse(format!(
            "failed to load chat template: {err}"
        ))),
    }
}

fn fallback_prompt(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = match message.role {
            ChatRole::System => "System",
            ChatRole::User => "User",
            ChatRole::Assistant => "Assistant",
        };
        out.push_str(role);
        out.push_str(": ");
        out.push_str(&message.content);
        out.push('\n');
    }
    out.push_str("Assistant: ");
    out
}

fn map_decode_error(err: DecodeError) -> AiError {
    AiError::UnexpectedResponse(format!("llama.cpp decode error: {err}"))
}

fn map_batch_add_error(err: BatchAddError) -> AiError {
    AiError::UnexpectedResponse(format!("llama.cpp batch error: {err}"))
}
