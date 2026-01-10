# Emacs setup (template)

This template configures Emacs to launch `nova-lsp` over stdio for `java-mode`.

## Prerequisites

- Emacs 28+
- `nova-lsp` available on your `$PATH`

## Option A: `eglot` (built-in in Emacs 29+)

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '(java-mode . ("nova-lsp" "--stdio"))))

(add-hook 'java-mode-hook #'eglot-ensure)
```

## Option B: `lsp-mode`

```elisp
(with-eval-after-load 'lsp-mode
  (lsp-register-client
   (make-lsp-client
    :new-connection (lsp-stdio-connection '("nova-lsp" "--stdio"))
    :activation-fn (lsp-activate-on "java")
    :server-id 'nova-lsp)))

(add-hook 'java-mode-hook #'lsp)
```

