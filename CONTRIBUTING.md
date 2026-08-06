# Contributing

Thanks for helping build this out. There are two ways to contribute a project, and the requirements differ, so pick your route first.

## Two ways to contribute

**Host your source here.** Your project becomes a directory in this repository with its own README, license, and build instructions. People can clone the repo and build it directly. Best for self-contained examples and reference implementations you want maximally easy to run.

**Link to your own repository.** Your project stays where it is and gets a one-line entry pointing at it. Best when you want to keep your own release cadence, issue tracker, and CI, or when the project is too large to vendor sensibly.

Neither route is second-class. If you are unsure, link: it is easier to move a linked project in later than to pull a hosted one back out.

## Where hosted projects go

One directory per project, under the category it belongs to:

```
components/                     Reusable components implementing a WIT interface
  mcp-servers/                  Model Context Protocol servers
host-plugins/
  native/                       Rust plugins implementing the HostPlugin trait
  component/                    Capabilities built as Wasm components
workload-examples/              End-to-end applications
tools/                          CLIs, libraries, editor integrations, tooling
```

A component that serves MCP goes in `components/mcp-servers/`. Any other reusable component goes directly in `components/`.

The two host plugin categories are easy to mix up. Sort it out by asking how your plugin is built:

- Written in **Rust and compiled into the host binary**, use `host-plugins/native/`.
- Shipped as a **Wasm component** loaded at runtime, use `host-plugins/component/`.

Use a short, lowercase, hyphenated directory name matching your project. Linked projects need no directory; they are index entries only.

## Licensing

**If you host source here, licensing is not optional.** Your code is copied into this repository and redistributed by it:

- **You must own the code, or have the right to contribute it.** Signing off your commits (below) is how you certify this.
- **Your project directory must contain a `LICENSE` file.** A directory with no license is all-rights-reserved by default, meaning nobody, including this project, can legally use, build, or redistribute it.
- **Use Apache-2.0 or a compatible permissive license** (MIT, BSD-2/3-Clause). This repository is Apache-2.0. Copyleft licenses create obligations we cannot take on for the repository as a whole, so they cannot be accepted.
- **Third-party code you vendor into your project** keeps its own license and attribution. Do not strip license headers.

**If you link instead, a license is strongly encouraged but not required.** We are not redistributing your code, so this is your call. Be aware that an unlicensed repository is all-rights-reserved by default, so nobody who finds it here can legally use or fork it. Adding Apache-2.0 or MIT is the cheapest thing you can do to make your project usable.

## What your project needs

Both routes:

- **A README** covering what it does, what it needs installed, how to build it, and how to run it.
- **A working build** from a clean checkout, using `wash build` or a documented equivalent. Say which Cosmonic Control or `wash` version you tested against.
- **Maintenance.** You maintain your project. Entries that stop building against current releases may be removed. That is upkeep, not a judgment of the work.

Hosted projects additionally:

- **Committed source, not artifacts.** No `target/`, `node_modules/`, `dist/`, or built `.wasm` files.
- **No large binaries.** Model weights, datasets, and audio or video fixtures belong behind a download script or a documented fetch step, not in git history. If your project needs a 400 MB model, ship a `fetch-models.sh` and document it in your README.

Work-in-progress and experimental projects are welcome. Say so in your README so readers know what they are getting.

## Adding your entry to the index

Add one line to the matching section of the root [README.md](README.md).

For a hosted project, link the directory and tag it:

```markdown
- [audio-summarizer](workload-examples/audio-summarizer/) (hosted): Transcribes audio and summarizes it with a local LLM, composed from three Rust components. Runs inference on-device and includes a Kubernetes deployment guide.
```

For a linked project, link the repository:

```markdown
- [audio-summarizer](https://github.com/someone/audio-summarizer) (linked): Transcribes audio and summarizes it with a local LLM, composed from three Rust components. Runs inference on-device and includes a Kubernetes deployment guide.
```

Guidelines:

- Describe what the project **does**, not why it is good. Skip "awesome", "simple", "powerful", "blazingly fast".
- Name the concrete pieces, such as languages, interfaces, and notable dependencies, so readers can tell whether it fits their use case.
- Keep it to two sentences.

Too vague:

```markdown
- [audio-summarizer](workload-examples/audio-summarizer/) (hosted): An awesome AI app for Cosmonic.
```

## Submitting

1. Fork this repository and create a branch.
2. Add your project directory if you are hosting source, plus your one-line index entry.
3. Open one pull request per project, titled `Add <project-name>`.
4. In the description, say what the project does and how you tested it.

### Sign your commits

This repository enforces the [Developer Certificate of Origin](https://developercertificate.org/). Every commit needs a `Signed-off-by` line, which `git` adds for you:

```console
git commit -s -m "Add audio-summarizer"
```

If the DCO check fails on an existing commit, amend it:

```console
git commit --amend -s --no-edit
git push --force-with-lease
```

Signing off certifies that you wrote the code or otherwise have the right to submit it under the project's license. If you are hosting source here, please take that seriously.

## Removing or updating a project

Open a pull request. If you are reporting a project that no longer builds and you do not maintain it, an issue is fine. No need to send the fix yourself.

## Getting help

Ask in the [community Slack](https://slack.wasmcloud.com) or open an issue. The [Cosmonic docs](https://cosmonic.com/docs/) cover installing Cosmonic Control, and the [`wash` developer guide](https://wasmcloud.com/docs/wash/developer-guide/) covers building and running workloads locally.
