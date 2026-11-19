// Generated from jimmy-proto.
export type Turn = number;
export type Mode = "wake" | "ptt" | "open";
export type Codec = "pcm16" | "opus";
export type Configure = { mode?: Mode, wake?: string, wake_reply?: boolean, language?: string, keywords?: Array<string>, voice?: string, rate?: number, input_rate?: number, output_codec?: Codec, endpoint_ms?: number, followup_ms?: number, echo_cancelled?: boolean, };
export type Word = { text: string, start: number, end: number, probability: number, };
export type Transcript = { text: string, language: string, words: Array<Word>, };
export type ClientEvent = { "type": "session.configure" } & Configure | { "type": "listen.start" } | { "type": "listen.stop" } | { "type": "speak.append", text: string, turn?: number | null, } | { "type": "speak.flush", turn?: number | null, } | { "type": "speak.stop", turn?: number | null, } | { "type": "cue.play", id: string, turn?: number | null, } | { "type": "cancel", turn?: number | null, } | { "type": "Unknown" };
export type EndReason = "done" | "cancelled" | "error";
export type ServerEvent = { "type": "session.ready", turn: number, protocol: number, models: Array<string>, voices: Array<string>, sample_rates: [number, number], } | { "type": "wake", turn: number, phrase: string, confidence: number, } | { "type": "speech.start", turn: number, } | { "type": "speech.end", turn: number, } | { "type": "transcript.partial", turn: number, committed: string, pending: string, } | { "type": "transcript.final", turn: number, text: string, language: string, words: Array<Word>, } | { "type": "speak.start", turn: number, sample_rate: number, codec: Codec, } | { "type": "speak.end", turn: number, reason: EndReason, } | { "type": "cue", turn: number, id: string, } | { "type": "error", turn?: number | null, code: string, message: string, fatal: boolean, };
