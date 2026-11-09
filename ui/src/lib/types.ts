// Mirrors crates/mimic-core (protocol.rs, config.rs, models.rs, rules.rs, chunk.rs).

export type DictationPhase = "idle" | "listening" | "hands-free" | "finishing";
export type ModelPhase = "not-installed" | "installing" | "unloaded" | "loading" | "ready" | "error";

export interface ModelStatus {
  id: string;
  name: string;
  phase: ModelPhase;
  progress: number | null;
  detail: string;
  device: string;
  memory_mb: number | null;
  unload_in_s: number | null;
  error: string | null;
}

export interface Status {
  version: string;
  pid: number;
  uptime_s: number;
  phase: DictationPhase;
  model: ModelStatus;
  mic: { device: string; open: boolean; sample_rate: number; error: string | null };
  stats: {
    dictations: number;
    words: number;
    audio_seconds: number;
    median_latency_ms: number | null;
    last_latency_ms: number | null;
  };
  onboarded: boolean;
  config_path: string;
  data_path: string;
  hotkeys: { push_to_talk: string; hands_free: string; add_rule: string; paused: boolean };
  install: InstallInfo;
  update: UpdateStatus;
  agent?: AgentStatus;
}

export interface ServiceStatus {
  phase: ModelPhase;
  detail: string;
  error: string | null;
  name: string;
}

export interface StreamStatus {
  label: string;
  device: string;
  open: boolean;
  error: string | null;
}

export interface AgentStatus {
  enabled: boolean;
  autopilot: boolean;
  talking: boolean;
  thinking: boolean;
  speaking: boolean;
  llm: ServiceStatus;
  tts: ServiceStatus;
  voice_s: number;
  voice_pinned: boolean;
  hear: StreamStatus[];
  route: StreamStatus;
  context: ContextDoc;
}

export type SectionKey = "me" | "style" | "people" | "situation" | "facts" | "rules" | "summary";

/** The agent's context document: items per section, in order. */
export interface ContextDoc {
  sections: [SectionKey, string[]][];
}

export interface AgentLine {
  id: number;
  at: number;
  who: "me" | "them" | "agent" | "private" | "note";
  source: string;
  text: string;
  interrupted: boolean;
  /** Which voice (on a call), and what to call it ("Speaker 2", or a name). */
  speaker?: number | null;
  name?: string;
}

export type AgentMode = "stand-in" | "fun" | "sidekick";
export interface AgentModes {
  stand_in: string;
  fun: string;
  sidekick: string;
}

export interface AgentVoice {
  id: number;
  name: string;
  label: string;
  seconds: number;
  last_heard: number;
}

export interface AgentSources {
  apps: string[];
  outputs: { name: string; default: boolean }[];
  inputs: { name: string; default: boolean }[];
}

export interface InstallInfo {
  /** "installed" | "dev" | "portable" */
  kind: string;
  dir: string;
  registered_dir: string | null;
  registered_version: string | null;
}

export interface UpdateStatus {
  current: string;
  latest: string | null;
  available: boolean;
  /** This version is below the supported minimum. */
  required: boolean;
  skipped: boolean;
  message: string | null;
  notes_url: string | null;
  checked_at: number | null;
  checking: boolean;
  downloading: number | null;
  error: string | null;
  auto: boolean;
  updated_from: string | null;
}

export interface Config {
  general: {
    autostart: boolean;
    autostart_delay_s: number;
    onboarded: boolean;
    sounds: boolean;
    sound_volume: number;
    history: boolean;
    check_updates: boolean;
    auto_update: boolean;
  };
  model: {
    id: string;
    device: "auto" | "gpu" | "cpu";
    residency: "always" | "idle-unload" | "on-demand";
    idle_unload_min: number;
    load_priority: "low" | "normal";
    threads: number;
    language: string;
    hotwords: boolean;
  };
  custom: {
    kind: "openai-transcriptions";
    base_url: string;
    api_key: string;
    model: string;
    live_preview: boolean;
    word_timestamps: boolean;
    timeout_s: number;
  };
  audio: {
    device: string;
    preroll_ms: number;
    postroll_ms: number;
    keep_mic_open: boolean;
    pause_ms: number;
    max_phrase_s: number;
    preview_interval_ms: number;
    gain: number;
  };
  hotkeys: {
    push_to_talk: string;
    hands_free: string;
    add_rule: string;
    cancel: string;
    double_tap_lock: boolean;
    double_tap_ms: number;
    agent_talk: string;
    agent_autopilot: string;
    agent_reply: string;
    agent_stop: string;
    sounds: string[];
  };
  output: {
    method: "type" | "paste";
    newline: "shift-enter" | "enter";
    continuation: boolean;
    continuation_window_s: number;
    seam_repair: boolean;
    smart_spacing: boolean;
  };
  text: {
    casing: "natural" | "lowercase";
    spoken_punctuation: boolean;
    remove_fillers: boolean;
    drop_final_period: boolean;
    apply_rules: boolean;
  };
  ui: {
    theme: "system" | "light" | "dark";
    overlay: boolean;
    overlay_position: "bottom" | "top";
    overlay_preview: boolean;
    keep_warm: boolean;
    reduce_motion: boolean;
  };
  agent: {
    enabled: boolean;
    mode: AgentMode;
    modes: AgentModes;
    wakeword: string;
    reply_after_ms: number;
    barge_in: "finish" | "stop" | "off";
    turn_detection: boolean;
    speaker_id: boolean;
    mute_mic_in_autopilot: boolean;
    context_lines: number;
    max_sentences: number;
    summarize_every: number;
    forget_after_min: number;
    history_min: number;
    notes_dir: string;
    llm: {
      provider: "local" | "custom";
      base_url: string;
      api_key: string;
      model: string;
      local_model: string;
      local_ctx: number;
      temperature: number;
      max_tokens: number;
      timeout_s: number;
    };
    tts: { model: string; experts: string; vram_layers: number; voice_s: number; pinned: boolean; temperature: number; unload_after_min: number };
    hear: { enabled: boolean; sources: string[]; pause_ms: number };
    route: { output: string; passthrough: boolean; mic_gain: number; voice_gain: number; monitor: boolean; monitor_device: string };
  };
  soundboard: {
    enabled: boolean;
    volume: number;
    monitor: boolean;
    slots: { file: string; volume: number; label: string }[];
  };
}

export interface ModelInfo {
  id: string;
  name: string;
  vendor: string;
  engine: "phonon" | "parakeet" | "custom";
  tagline: string;
  languages: string;
  multilingual: boolean;
  download_mb: number;
  memory_mb: number;
  gpu: boolean;
  recommended: boolean;
  source: string;
  installed: boolean;
}

export interface Rule {
  target: string;
  variants: string[];
  created: number;
  uses: number;
}

export interface SearchHit {
  rule: Rule;
  score: number;
  matched_variant: string | null;
}

export interface UpsertOutcome {
  rule: Rule;
  created: boolean;
  moved: [string, string][];
  existing: string[];
}

export interface ChunkGuess {
  context: string[];
  start: number;
  end: number;
  text: string;
}

export interface PopupContext {
  chunk: ChunkGuess;
  return_hwnd: number;
  from_dictation: boolean;
  can_apply_last: boolean;
}

export interface HistoryItem {
  id: number;
  at: number;
  text: string;
  raw: string;
  app: string;
  duration_s: number;
  latency_ms: number;
}

export interface InstallProgress {
  id: string;
  step: string;
  progress: number | null;
  done: boolean;
  error: string | null;
}

export interface DeviceInfo {
  name: string;
  default: boolean;
}

export interface DaemonEvent {
  event: string;
  data: any;
}
