# CursorAssist

Select text in any Windows app, click the small dot that appears, and refine, shorten, fix, change the tone of, or turn it into a prompt. Or describe a task, typed or dictated. Everything runs on local models on your own machine; no text or audio leaves the computer.

Built with Tauri 2: a Rust core plus a plain TypeScript and CSS UI. See [SCOPE.md](SCOPE.md) for the design, the phase results and the privacy and security model.

## What it uses

| Part | Default |
|---|---|
| Quick actions (grammar, shorten, tone) | Gemma 4 E2B, QAT q4_0 GGUF |
| Refine, prompt, tasks | Qwen3-4B-Instruct-2507, Q4_K_M GGUF |
| Dictation | Parakeet TDT 0.6B v3, ggml q8_0 |
| Model server | `llama-server` (llama.cpp, Vulkan build) |
| Speech server | `parakeet-server` (whisper.cpp Parakeet support) |

Model paths, ports, GPU use and idle times are in `%APPDATA%\com.rib.cursorassist\settings.json`. Actions, with their prompts and which model runs them, are in `actions.json` next to it. Both files are created with defaults on first run, and edits to `actions.json` apply without a restart.

## Building

Requirements: Windows 10/11, Rust (stable, MSVC), Node 20+, and the Visual Studio C++ build tools.

The installer bundles the two helper servers. They are not in this repository, so put these files in `src-tauri/resources/bin/` first:

- `llama-server.exe` and `parakeet-server.exe` (Vulkan builds)
- `msvcp140.dll`, `vcruntime140.dll`, `vcruntime140_1.dll` and `vcomp140.dll`, from the Visual Studio redistributable folder (`VC\Redist\MSVC\<version>\x64`)

Then:

```bash
npm install
npm run tauri dev
```

```bash
npm run tauri build
```

The installer is written to `src-tauri/target/release/bundle/nsis/`. It installs per user without admin rights, and CursorAssist registers itself to start with Windows (toggle this from the tray menu).

Models are not bundled. Download the GGUF files and point `settings.json` at them.

## Using it

- Select text, then click the dot that appears next to it.
- Recent results: the menu's **Recent** row, the tray menu, or **Ctrl+Alt+H**. They are kept in memory only.
- **Pause** in the tray menu turns it off temporarily.
