-- Minimal Neovim LSP setup for `nova-lsp`.
--
-- This file is meant to be copy/pasted into your own Neovim config. It assumes
-- you have `neovim/nvim-lspconfig` installed and `nova-lsp` available on $PATH.

local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')
local util = require('lspconfig.util')

-- `nvim-lspconfig` doesn't ship a built-in `nova-lsp` config, so define one if
-- it doesn't already exist.
if not configs.nova_lsp then
  configs.nova_lsp = {
    default_config = {
      cmd = { 'nova-lsp', '--stdio' },
      filetypes = { 'java' },
      root_dir = function(fname)
        return util.root_pattern(
          'pom.xml',
          'build.gradle',
          'build.gradle.kts',
          'settings.gradle',
          'settings.gradle.kts',
          'MODULE.bazel',
          'WORKSPACE',
          'WORKSPACE.bazel',
          '.git',
          '.nova'
        )(fname) or util.path.dirname(fname)
      end,
      single_file_support = true,
    },
  }
end

lspconfig.nova_lsp.setup({})

-- Organize imports via the standard LSP code action kind.
vim.api.nvim_create_user_command('NovaOrganizeImports', function()
  vim.lsp.buf.code_action({
    context = { only = { 'source.organizeImports' } },
  })
end, { desc = 'Organize imports (source.organizeImports)' })

