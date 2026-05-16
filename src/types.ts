// Mirror of the Rust → webview event payloads. Kept in sync with src-tauri/src/events.rs.

export type HudState = "idle" | "listening" | "thinking" | "speaking";

export interface StatePayload {
  state: HudState;
}

export interface AmplitudePayload {
  mic: number; // 0..1
  tts: number; // 0..1
}

export interface TranscriptPayload {
  text: string;
}

export interface CaptionPayload {
  text: string;
  final: boolean;
}

export interface StatusPayload {
  text: string;
}

export interface AgentPayload {
  action: "start" | "tool" | "done";
  id: string;
  parentId?: string;
  subagent?: string;
  tool?: string;
  desc?: string;
}

export interface ErrorPayload {
  message: string;
}
