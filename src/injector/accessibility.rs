// GUI auto-paste support has been removed.
//
// The autopilot binary now targets local AI CLIs (`claude`, `codex`) and
// HTTP APIs only — no GUI apps. Long pipelines run reliably via cli mode
// (subprocess per stage) or api mode (REST per stage). Webhook self-chain
// remains for backwards compatibility but is no longer driven by clipboard
// keystrokes.
