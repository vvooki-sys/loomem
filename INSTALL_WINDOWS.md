# Building Loomem on Windows

Build Loomem from source on Windows using **Git Bash**. There are no prebuilt Windows binaries yet, so this guide compiles `loomem-server` and the other workspace crates with the MSVC toolchain.

> **Status: experimental.** Windows is not a release target and is not covered by CI. Expect rough edges; issues and PRs are welcome.

## Prerequisites

Install these once. Each step is independent — run them in order, then **open a new Git Bash window** so the updated `PATH` is picked up.

**1. Git for Windows** (includes Git Bash): download from [git-scm.com/install/windows](https://git-scm.com/install/windows).

**2. Visual Studio Build Tools with the C++ workload** (the MSVC compiler and linker that Rust and RocksDB need):

```bash
winget install Microsoft.VisualStudio.2022.BuildTools \
  --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

The `--override` part matters: without it, Build Tools installs with no C++ workload and the build fails at link time.

**3. Rust** (stable, via rustup):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://win.rustup.rs/x86_64 -o rustup-init.exe
./rustup-init.exe
```

Accept the defaults. The installer adds `%USERPROFILE%\.cargo\bin` to your Windows `PATH`. Check it in a new Git Bash window:

```bash
cargo --version
```

**4. LLVM** (provides `libclang`, required for the RocksDB build):

```bash
winget install LLVM.LLVM
```

**5. Point the build at `libclang`.** Add this to `~/.bashrc` so it survives new shells (adjust the path if you installed LLVM elsewhere):

```bash
echo 'export LIBCLANG_PATH="/c/Program Files/LLVM/bin"' >> ~/.bashrc
echo 'export PATH="$LIBCLANG_PATH:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

## Build from source

```bash
git clone https://github.com/vvooki-sys/loomem.git
cd loomem
cp entities.toml.example entities.toml   # personal entity config (gitignored)
cargo build --release -p loomem-server
```

The first build compiles RocksDB (C++) and takes a while. The binary lands in `target/release/loomem-server.exe`, unless your global cargo config redirects `target-dir`.

## Run

```bash
cargo run --release -p loomem-server
# server listens on http://127.0.0.1:3030, data in ./data
```

Check it's alive (in another Git Bash window):

```bash
curl http://localhost:3030/health
```

Stop the server with **Ctrl+C** (closing the console window also shuts it down gracefully).

## Connect an MCP client

Same as on macOS and Linux — see [Connect an MCP client](README.md#connect-an-mcp-client) in the README. For Claude Code:

```bash
claude mcp add --transport http loomem http://localhost:3030/mcp
```

For the Claude desktop app, the config file is `%APPDATA%\Claude\claude_desktop_config.json`, and the `command` must be the absolute Windows path to `loomem-cli.exe`.

## Troubleshooting

- **`Unable to find libclang`** — `LIBCLANG_PATH` is unset or wrong. Run `ls "$LIBCLANG_PATH"/libclang.dll` to check it.
- **`link.exe not found`** or MSVC linker errors — the Build Tools C++ workload is missing. Re-run step 2 with the `--override` flag.
- **`cargo: command not found`** — open a new Git Bash window after installing Rust, so the updated `PATH` is loaded.
- **Disk-space guard** — the startup disk-space check is not implemented on Windows; the server logs a warning and continues.
