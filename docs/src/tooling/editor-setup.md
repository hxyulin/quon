# Editor Setup

Before configuring your editor, run `quon configure` to generate the
`rust-project.json` file. Without it, rust-analyzer has no project model to
work with.

## VS Code

Install the
[rust-analyzer extension](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer).

After running `quon configure`, VS Code should automatically detect the
`rust-project.json` in the project root. If it does not, add the following to
`.vscode/settings.json`:

```json
{
    "rust-analyzer.linkedProjects": ["rust-project.json"]
}
```

## Neovim

If using [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig), configure
rust-analyzer to use the project file:

```lua
require('lspconfig').rust_analyzer.setup {
    settings = {
        ['rust-analyzer'] = {
            linkedProjects = { 'rust-project.json' },
        },
    },
}
```

If you use a different LSP client, the key setting is the same: point
`linkedProjects` at your `rust-project.json` path.

### Quon build script (`quon.rs`)

Since the Rust DSL switch, Quon's build script is regular Rust.
rust-analyzer provides completions, hover, goto-def, and live type
errors out of the box — no Quon-specific LSP is required. Make
sure you've run `quon configure` so the generated `rust-project.json`
includes the `quon.rs` crate entry.

## Helix

Add the following to `.helix/languages.toml` in your project root:

```toml
[language-server.rust-analyzer.config]
linkedProjects = ["rust-project.json"]
```

## Zed

Zed supports rust-analyzer natively. Add to your project's `.zed/settings.json`:

```json
{
    "lsp": {
        "rust-analyzer": {
            "initialization_options": {
                "linkedProjects": ["rust-project.json"]
            }
        }
    }
}
```

## General notes

- Run `quon configure` **before** opening the editor so the JSON file exists
  when rust-analyzer starts.
- If rust-analyzer shows errors about missing crates, try re-running `quon
  configure` followed by restarting the language server (most editors have a
  "Restart Server" command).
- The generated config crate gets a stub `lib.rs` during `quon configure`.
  This stub is replaced with real generated content on the next `quon build`.
  After building, re-run `quon configure` and restart the language server to
  pick up the real constants.
- If you change the output path with `quon configure --output`, update your
  editor's `linkedProjects` setting to match.
