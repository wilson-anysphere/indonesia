;;; nova.el --- Nova LSP configuration (template) -*- lexical-binding: t; -*-

;; This file is a minimal template for connecting Emacs to `nova-lsp` for Java.
;;
;; Installation (one option):
;;   1) Copy this file somewhere on your `load-path`
;;   2) Add: (require 'nova)

;;; Option A: eglot (recommended)
;;
;; Eglot is built-in in Emacs 29+ and is available from GNU ELPA for Emacs 28.
(when (require 'eglot nil t)
  (add-to-list 'eglot-server-programs
               '(java-mode . ("nova-lsp" "--stdio")))
  (add-hook 'java-mode-hook #'eglot-ensure))

;;; Option B: lsp-mode (optional)
;;
;; Uncomment if you prefer `lsp-mode` instead of eglot.
;;
;; (with-eval-after-load 'lsp-mode
;;   (lsp-register-client
;;    (make-lsp-client
;;     :new-connection (lsp-stdio-connection '("nova-lsp" "--stdio"))
;;     :major-modes '(java-mode)
;;     :server-id 'nova-lsp)))
;;
;; (add-hook 'java-mode-hook #'lsp)

;;; Organize imports
;;
;; Uses the standard LSP code action kind `source.organizeImports`.
(defun nova-organize-imports ()
  "Organize imports in the current buffer via LSP code actions."
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

