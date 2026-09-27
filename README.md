# Voice Chat

A desktop app for real-time, hands-free voice conversations with customizable AI characters, built on [Tauri 2](https://tauri.app/) and Alibaba Cloud's [Model Studio](https://help.aliyun.com/zh/model-studio/) (DashScope) realtime audio API.

Talk to it like a phone call: open the mic once and keep talking. The app detects when you've finished a sentence, sends it automatically, and you can interrupt the reply just by speaking over it.

## Download

Windows installers are attached to [GitHub Releases](https://github.com/FINTERIGION/voice-chat/releases/latest). On first launch, open **Settings**, paste a [Model Studio API key](https://bailian.console.aliyun.com/), and use **Test connectivity** to confirm it works.

## Features

**Conversation**

- Hands-free turn-taking via server-side voice activity detection
- Barge-in: start speaking while the AI is talking and it stops immediately
- Desktop subtitle: an always-on-top, click-through caption with optional translation

**Characters**

- Define persona, speech habits, response language, and voice
- Optional one-line description to full persona expansion via LLM
- Share a character with someone else as a single file

**Long-term memory**

- Each conversation is summarized into a rolling summary plus discrete facts
- Per-character on/off, plus a per-conversation "don't record this one" toggle
- Memories are browsable, editable, and deletable from the UI

**Voice Studio**

- Clone a voice from an in-app recording or a local audio file
- Design a voice from a text description, preview it, then commit
- Manage custom voices stored under your DashScope account



## Requirements

- **Windows** — the API key is stored in the Windows Credential Manager
- [Node.js](https://nodejs.org/) 20+
- [Rust](https://www.rust-lang.org/tools/install) 1.85+ (edition 2024)



## Getting Started

```bash
npm install
npm run tauri dev
```

To build a release bundle:

```bash
npm run tauri build
```

## Models Used


| Purpose                                                      | Model                           |
| ------------------------------------------------------------ | ------------------------------- |
| Realtime speech conversation                                 | `qwen-audio-3.0-realtime-flash` |
| Memory summarization, conversation naming, persona expansion | `qwen3.8-flash`                 |
| Voice design previews                                        | `cosyvoice-v3.5-plus`           |
| Voice cloning                                                | `voice-enrollment`              |
| Avatar generation                                            | `qwen-image-3.0`                |


