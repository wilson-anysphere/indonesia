# Emacs setup (template)

This template configures Emacs to launch `nova-lsp` over stdio for `java-mode`.

The repo includes a copy/paste-ready config file at [`editors/emacs/nova.el`](./nova.el).

## Prerequisites

- Emacs 28+
- `nova-lsp` available on your `$PATH`

## Quick start

1. Copy [`editors/emacs/nova.el`](./nova.el) somewhere on your `load-path`, for example:
   `~/.emacs.d/lisp/nova.el`
2. Add the directory to `load-path` and load the template:

```elisp
(add-to-list 'load-path (expand-file-name "~/.emacs.d/lisp"))
(require 'nova) ;; or (load-file "/path/to/editors/emacs/nova.el")
```

## Project root detection (optional, recommended for non-git workspaces)

Emacs' built-in project system (`project.el`) is often VCS-based. If you open a Maven/Gradle/Bazel
workspace that is not checked into git (or if you're opening a nested file), `eglot`/`lsp-mode` may
start `nova-lsp` with the wrong workspace root.

The template provides an opt-in helper that treats common build-system marker files as project
roots (`pom.xml`, `build.gradle(.kts)`, `settings.gradle(.kts)`, `WORKSPACE(.bazel)`, `MODULE.bazel`):

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
               '(java-mode . ("nova-lsp" "--stdio"))))

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
    :new-connection (lsp-stdio-connection '("nova-lsp" "--stdio"))
    :activation-fn (lsp-activate-on "java")
    :server-id 'nova-lsp)))

(add-hook 'java-mode-hook #'lsp)
```

## Organize imports (optional)

```elisp
(global-set-key (kbd "C-c o") #'nova-organize-imports)
```
