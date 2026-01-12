# Emacs setup (template)

This template configures Emacs to launch Nova's LSP server over stdio for `java-mode`.

The repo includes a copy/paste-ready config file at [`editors/emacs/nova.el`](./nova.el).

## Prerequisites

- Emacs 28+
- `nova` available on your `$PATH` (recommended), or `nova-lsp` if you prefer to run the server binary directly.

## Quick start

1. Copy [`editors/emacs/nova.el`](./nova.el) somewhere on your `load-path`, for example:
   `~/.emacs.d/lisp/nova.el`
2. Add the directory to `load-path` and load the template:

```elisp
(add-to-list 'load-path (expand-file-name "~/.emacs.d/lisp"))
(require 'nova) ;; or (load-file "/path/to/editors/emacs/nova.el")
```

## AI multi-token completions (server-side overrides)

Nova’s **multi-token completions** are computed asynchronously by the server and surfaced via
`nova/completion/more` (see [`docs/protocol-extensions.md`](../../docs/protocol-extensions.md)).

If you want to control or disable these completions without changing `nova.toml`, set the server
startup environment variable `NOVA_AI_COMPLETIONS_MAX_ITEMS` before starting the language server.
This is read at server startup, so you must restart the server for changes to take effect.

For example, to disable multi-token completions entirely:

```elisp
(setenv "NOVA_AI_COMPLETIONS_MAX_ITEMS" "0")
```

## Project root detection (optional, recommended for non-git workspaces)

Emacs' built-in project system (`project.el`) is often VCS-based. If you open a Maven/Gradle/Bazel
workspace that is not checked into git (or if you're opening a nested file), `eglot`/`lsp-mode` may
start the server with the wrong workspace root.

The template provides an opt-in helper that treats common Nova/build-system marker files as project
roots (`nova.toml`, `.nova/`, `pom.xml`, `build.gradle(.kts)`, `settings.gradle(.kts)`, `WORKSPACE(.bazel)`, `MODULE.bazel`):

```elisp
(nova-project-root-setup)
```

## Option A: `eglot` (built-in in Emacs 29+)

```elisp
(nova-eglot-setup)
```

Or, inline:

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '(java-mode . ("nova" "lsp"))))

(add-hook 'java-mode-hook #'eglot-ensure)
```

## Option B: `lsp-mode`

```elisp
(nova-lsp-mode-setup)
```

Or, inline:

```elisp
(with-eval-after-load 'lsp-mode
  (lsp-register-client
   (make-lsp-client
    :new-connection (lsp-stdio-connection '("nova" "lsp"))
    :activation-fn (lsp-activate-on "java")
    :server-id 'nova-lsp)))

(add-hook 'java-mode-hook #'lsp)
```

## Organize imports (optional)

```elisp
(global-set-key (kbd "C-c o") #'nova-organize-imports)
```
