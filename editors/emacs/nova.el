;;; nova.el --- Nova LSP configuration (template) -*- lexical-binding: t; -*-

;; This is a minimal template that connects Emacs to `nova-lsp` for Java.
;;
;; Copy this file somewhere on your `load-path` (for example `~/.emacs.d/lisp/nova.el`)
;; and then, in your init:
;;   (require 'nova)
;;   (nova-eglot-setup)      ;; recommended (Emacs 29+)
;; or:
;;   (nova-lsp-mode-setup)   ;; if you prefer lsp-mode

(defgroup nova nil
  "Run the Nova language server."
  :group 'tools)

(defcustom nova-lsp-command '("nova-lsp" "--stdio")
  "Command used to start `nova-lsp`."
  :type '(repeat string)
  :group 'nova)

;; --- Project root detection -------------------------------------------------
;;
;; By default, `eglot` (and often `lsp-mode`) uses Emacs' `project.el` to decide the workspace root.
;; If a workspace is not in VCS, `project-current` may not find a project root automatically.
;;
;; This helper lets users opt into treating common Java build-system marker files as project roots
;; so `nova-lsp` starts with the correct workspace root even for non-git checkouts.
(defcustom nova-project-root-files
  '(
    ;; Nova config (works for "simple" projects without build tools).
    "nova.toml"
    ".nova.toml"
    "nova.config.toml"
    ".nova/config.toml"
    ".nova"
    ;; Maven
    "pom.xml"
    ;; Gradle (Groovy + Kotlin DSL)
    "build.gradle"
    "build.gradle.kts"
    "settings.gradle"
    "settings.gradle.kts"
    ;; Bazel
    "WORKSPACE"
    "WORKSPACE.bazel"
    "MODULE.bazel"
    ;; Fallback for VCS roots.
    ".git"
    )
  "Marker files used to infer a project root for Nova (via `project-find-functions`)."
  :type '(repeat string)
  :group 'nova)

(defun nova--project-root (dir)
  "Return the nearest ancestor of DIR containing a known Nova project root marker."
  (let ((root
         (locate-dominating-file
          dir
          (lambda (parent)
            (catch 'found
              (dolist (marker nova-project-root-files)
                (when (file-exists-p (expand-file-name marker parent))
                  (throw 'found t)))
              nil)))))
    (when root
      (expand-file-name root))))

;;;###autoload
(defun nova-project-root-setup ()
  "Register build-system-based project root detection for Nova.

This adds a `project-find-functions` entry that treats directories containing a marker from
`nova-project-root-files` as a `transient` project."
  (require 'project)
  (add-hook 'project-find-functions
            (lambda (dir)
              (let ((root (nova--project-root dir)))
                (when root
                  (cons 'transient root))))
            ;; Append so VCS detection (if available) wins first.
            t))

;;;###autoload
(defun nova-eglot-setup ()
  "Configure `eglot` to use `nova-lsp` for Java."
  (with-eval-after-load 'eglot
    (add-to-list 'eglot-server-programs
                 `((java-mode java-ts-mode) . ,nova-lsp-command)))

  (add-hook 'java-mode-hook #'eglot-ensure)
  (when (boundp 'java-ts-mode-hook)
    (add-hook 'java-ts-mode-hook #'eglot-ensure)))

;;;###autoload
(defun nova-lsp-mode-setup ()
  "Configure `lsp-mode` to use `nova-lsp` for Java."
  (with-eval-after-load 'lsp-mode
    (lsp-register-client
     (make-lsp-client
      :new-connection (lsp-stdio-connection (lambda () nova-lsp-command))
      :activation-fn (lsp-activate-on "java")
      :server-id 'nova-lsp)))

  (add-hook 'java-mode-hook #'lsp)
  (when (boundp 'java-ts-mode-hook)
    (add-hook 'java-ts-mode-hook #'lsp)))

;; Uses the standard LSP code action kind `source.organizeImports` (when available).
(defun nova-organize-imports ()
  "Organize imports in the current buffer via the active LSP client."
  (interactive)
  (cond
   ((and (fboundp 'eglot-code-action-organize-imports)
         (ignore-errors (eglot-current-server)))
    (eglot-code-action-organize-imports))
   ((and (fboundp 'lsp-organize-imports)
         (bound-and-true-p lsp-mode))
    (lsp-organize-imports))
   (t
    (user-error "No active LSP client (eglot or lsp-mode)"))))

(provide 'nova)

;;; nova.el ends here
