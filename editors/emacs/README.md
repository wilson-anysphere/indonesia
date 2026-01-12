# Emacs setup (template)

This template configures Emacs to launch `nova-lsp` over stdio for `java-mode`.

The repo includes a copy/paste-ready config file at [`editors/emacs/nova.el`](./nova.el).

## Prerequisites

- Emacs 28+
- `nova-lsp` available on your `$PATH`

## Option A: `eglot` (built-in in Emacs 29+)

```elisp
(require 'nova) ;; or (load-file "/path/to/editors/emacs/nova.el")
```

## Option B: `lsp-mode`

```elisp
;; See `editors/emacs/nova.el` for an optional `lsp-mode` snippet.
```

## Organize imports

```elisp
(global-set-key (kbd "C-c o") #'nova-organize-imports)
```
