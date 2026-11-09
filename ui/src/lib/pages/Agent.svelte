<script lang="ts">
  import { onMount, tick } from "svelte";
  import { fade, slide } from "svelte/transition";
  import { call } from "../backend";
  import Keys from "../components/Keys.svelte";
  import PageHeader from "../components/PageHeader.svelte";
  import Segmented from "../components/Segmented.svelte";
  import Select from "../components/Select.svelte";
  import Setting from "../components/Setting.svelte";
  import Slider from "../components/Slider.svelte";
  import Toggle from "../components/Toggle.svelte";
  import { app, loadAgentLines, setCfg, toast } from "../state.svelte";
  import type { AgentLine, AgentMode, AgentModes, AgentSources, AgentStatus, AgentVoice, SectionKey, ServiceStatus, StreamStatus } from "../types";

  const c = $derived(app.config!);
  const a = $derived(c.agent);
  const st = $derived<AgentStatus | undefined>(app.status?.agent);

  let tab = $state<"live" | "setup">("live");
  let sources = $state<AgentSources | null>(null);
  let brief = $state("");
  let showKey = $state(false);
  let log: HTMLDivElement | undefined = $state();
  let modeDefaults = $state<AgentModes | null>(null);
  let voices = $state<AgentVoice[]>([]);

  const MODES: { value: AgentMode; key: keyof AgentModes; label: string; blurb: string }[] = [
    { value: "stand-in", key: "stand_in", label: "Stand-in", blurb: "Sounds like you, keeps it moving, commits to nothing." },
    { value: "fun", key: "fun", label: "Fun", blurb: "You among friends: playful, quick, laughs along." },
    { value: "sidekick", key: "sidekick", label: "Sidekick", blurb: "Its own personality, in your voice. Open about being an AI." },
  ];
  const mode = $derived(MODES.find((m) => m.value === a.mode) ?? MODES[0]);
  const modeText = $derived(a.modes?.[mode.key] ?? "");
  const modeEdited = $derived(!!modeDefaults && modeText !== modeDefaults[mode.key]);

  async function loadVoices() {
    try {
      voices = await call<AgentVoice[]>("agent.speakers");
    } catch {
      voices = [];
    }
  }

  async function loadSources() {
    try {
      sources = await call<AgentSources>("agent.sources");
    } catch {
      sources = null;
    }
  }

  onMount(() => {
    loadAgentLines();
    loadSources();
    loadVoices();
    call<AgentModes>("agent.modes.defaults")
      .then((d) => (modeDefaults = d))
      .catch(() => {});
  });

  // What was said on the call, and (separately) what you told it privately and what it noted.
  const SPOKEN = new Set(["me", "them", "agent"]);
  const spoken = $derived(app.agentLines.filter((l) => SPOKEN.has(l.who)));
  const told = $derived(app.agentLines.filter((l) => !SPOKEN.has(l.who)).slice(-4));

  let voicesDue: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    spoken.length;
    tick().then(() => log?.scrollTo({ top: log.scrollHeight }));
    // A new line may be a new voice (or a renamed one).
    clearTimeout(voicesDue);
    voicesDue = setTimeout(loadVoices, 400);
  });

  async function renameVoice(v: AgentVoice, name: string) {
    await run("agent.speakers.rename", { id: v.id, name: name.trim() });
    loadVoices();
    loadAgentLines();
  }
  async function forgetVoice(v: AgentVoice) {
    await run("agent.speakers.forget", { id: v.id });
    loadVoices();
  }

  async function run(method: string, params: Record<string, unknown> = {}) {
    try {
      await call(method, params);
    } catch (e) {
      toast(String(e instanceof Error ? e.message : e), "error");
    }
  }

  /** Same as saying it while holding the talk shortcut: routed to the right place automatically. */
  async function sendBrief() {
    const t = brief.trim();
    if (!t) return;
    brief = "";
    await run("agent.tell", { text: t });
  }

  const items = (key: SectionKey) => st?.context.sections.find((s) => s[0] === key)?.[1] ?? [];
  const about = $derived([...items("me").map((t) => ({ key: "me" as SectionKey, t })), ...items("style").map((t) => ({ key: "style" as SectionKey, t }))]);
  const KIND: Partial<Record<SectionKey, string>> = { me: "you", style: "voice", people: "person", situation: "situation", facts: "fact", rules: "rule" };
  const session = $derived((["people", "situation", "facts", "rules"] as SectionKey[]).flatMap((key) => items(key).map((t) => ({ key, t }))));
  const summary = $derived(items("summary")[0] ?? "");

  function svc(s: ServiceStatus | undefined): { text: string; tone: string } {
    if (!s) return { text: "—", tone: "dim" };
    switch (s.phase) {
      case "ready":
        return { text: [s.name, s.detail].filter(Boolean).join(" · ") || "Ready", tone: "ok" };
      case "loading":
        return { text: s.detail || "Loading", tone: "busy" };
      case "error":
        return { text: s.error ?? "Error", tone: "bad" };
      case "not-installed":
        return { text: s.error ?? "Not installed", tone: "bad" };
      default:
        return { text: s.detail === "idle" ? "Asleep (loads when needed)" : "Off", tone: "dim" };
    }
  }
  function dev(s: StreamStatus | undefined, off: string): { text: string; tone: string } {
    if (!s || (!s.open && !s.error)) return { text: off, tone: "dim" };
    if (s.error) return { text: s.error, tone: "bad" };
    return { text: s.device.replace(/^file:/, "file: "), tone: "ok" };
  }

  const sourceLabel = (s: string) => (s.startsWith("app:") ? s.slice(4) : s === "default" ? "Default output" : s.replace(/^device:/, "").replace(/ \(.*\)$/, ""));
  const addableSources = $derived(
    [
      ...(sources?.apps ?? []).map((n) => ({ value: `app:${n}`, label: n, hint: "app" })),
      { value: "default", label: "Default output", hint: "everything you hear" },
      ...(sources?.outputs ?? []).map((o) => ({ value: `device:${o.name}`, label: o.name, hint: "output" })),
    ].filter((o) => !a.hear.sources.includes(o.value)),
  );
  const outputs = $derived(sources?.outputs ?? []);
  const routeOptions = $derived([{ value: "", label: "None", hint: "the agent can't talk through your mic" }, ...outputs.map((o) => ({ value: o.name, label: o.name }))]);
  const monitorOptions = $derived([{ value: "", label: "Windows default output" }, ...outputs.map((o) => ({ value: o.name, label: o.name }))]);

  const who: Record<string, string> = { me: "YOU", them: "THEM", agent: "AGENT", private: "YOU, PRIVATELY", note: "NOTED" };
  const speakerName = (l: AgentLine) => (l.who === "them" && l.name ? l.name : (who[l.who] ?? l.who));
  const secs = (n: number) => (n >= 60 ? `${Math.round(n / 60)} min` : `${Math.round(n)} s`);
  const time = (ms: number) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const vram = (n: number) => (n === 0 || n >= 40 ? "all" : `${n}/40`);
  const mins = (n: number) => (n === 0 ? "never" : `${n} min`);
</script>

<PageHeader index="02" title="Agent" subtitle="Brief it, and it answers for you in your own voice.">
  {#snippet actions()}
    <Segmented
      value={tab}
      options={[
        { value: "live", label: "Live" },
        { value: "setup", label: "Setup" },
      ]}
      onchange={(v) => (tab = v as "live" | "setup")}
    />
  {/snippet}
</PageHeader>

{#if tab === "live"}
  <div class="cols live" in:fade={{ duration: 120 }}>
    <div class="col">
      <div class="group">
        <span class="label">Agent</span>
        <Setting title="Agent on" hint="Follows the conversation and keeps a sample of your voice. Off frees the GPU.">
          <Toggle checked={a.enabled} onchange={(v) => setCfg("agent.enabled", v)} />
        </Setting>
        {#if a.enabled}
          <div class="now" transition:slide={{ duration: 140 }}>
            <button class="pilot" class:on={st?.autopilot} onclick={() => run("agent.autopilot", { on: !st?.autopilot })}>
              <span class="label">Autopilot</span>
              <span class="state">{st?.autopilot ? "Answering for you" : "Off"}</span>
              <Keys combo={c.hotkeys.agent_autopilot} size="sm" />
            </button>
            <div class="acts">
              <button class="btn sm" onclick={() => run("agent.reply")}>Answer now</button>
              <button class="btn sm" onclick={() => run("agent.stop")}>Stop</button>
              <span class="mono activity">
                {#if st?.talking}listening to you{:else if st?.speaking}speaking{:else if st?.thinking}thinking{:else}ready{/if}
              </span>
            </div>
          </div>
        {/if}
      </div>

      {#if a.enabled}
        <div class="group grow" in:fade={{ duration: 120 }}>
          <div class="head">
            <span class="label">Conversation</span>
            <span class="mono faint">forgotten after {mins(a.forget_after_min)} of quiet</span>
          </div>
          <div class="log" bind:this={log}>
            {#each spoken as l (l.id)}
              <div class="line {l.who}" class:named={l.who === "them" && !!l.name}>
                <span class="who mono" title={l.source ? `heard on ${l.source}` : ""}>{speakerName(l)}{#if l.source && !l.name}<span class="src"> · {l.source}</span>{/if}</span>
                <span class="text">{l.text}{#if l.interrupted}<span class="cut mono"> cut off</span>{/if}</span>
                <span class="at mono">{time(l.at)}</span>
              </div>
            {:else}
              <p class="muted empty">Quiet so far. What you and the others say shows up here.</p>
            {/each}
          </div>
        </div>
      {/if}
    </div>

    <div class="col">
      <div class="group">
        <div class="head">
          <span class="label">Setting</span>
          {#if modeEdited}
            <button class="link mono" onclick={() => modeDefaults && setCfg(`agent.modes.${mode.key}`, modeDefaults[mode.key])}>Reset text</button>
          {/if}
        </div>
        <div class="modes">
          {#each MODES as m (m.value)}
            <button class="mode" class:on={a.mode === m.value} onclick={() => setCfg("agent.mode", m.value)}>
              <span class="mname">{m.label}</span>
              <span class="mblurb">{m.blurb}</span>
            </button>
          {/each}
        </div>
        <textarea
          class="scenario"
          rows="3"
          value={modeText}
          placeholder="Describe the setting: who it is, who it talks to, how it should come across."
          onchange={(e) => setCfg(`agent.modes.${mode.key}`, e.currentTarget.value)}
        ></textarea>
        <p class="muted hint">Kept across conversations. The brief below is for this one.</p>
      </div>

      <div class="group">
        <span class="label">Brief it</span>
        <textarea
          class="brief"
          rows="2"
          placeholder="Tell it anything, the way you'd say it: “this is Alex, my manager”, “we're sorting out the release date”, “keep it short”, “don't promise anything”."
          bind:value={brief}
          onkeydown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              sendBrief();
            }
          }}
        ></textarea>
        <p class="muted hint">Enter to send. Or hold <Keys combo={c.hotkeys.agent_talk} size="sm" /> and say it, or start with “{a.wakeword || "mimic"}, …”.</p>
        {#if told.length}
          <div class="told">
            {#each told as l (l.id)}
              <div class="tline {l.who}" transition:slide={{ duration: 120 }}>
                <span class="who mono">{who[l.who]}</span>
                <span class="t">{l.text}</span>
                <span class="at mono">{time(l.at)}</span>
              </div>
            {/each}
          </div>
        {/if}
      </div>

      <div class="group">
        <div class="head">
          <span class="label">This conversation</span>
          {#if session.length || summary}
            <button class="link mono" onclick={() => run("agent.context.new")}>Forget now</button>
          {/if}
        </div>
        {#if summary}
          <p class="summary">{summary}</p>
        {/if}
        {#each session as it (it.key + it.t)}
          <div class="fact" transition:slide={{ duration: 120 }}>
            <span class="kind mono">{KIND[it.key]}</span>
            <span class="t">{it.t}</span>
            <button class="x mono" title="Forget" onclick={() => run("agent.context.remove", { section: it.key, text: it.t })}>×</button>
          </div>
        {:else}
          {#if !summary}<p class="muted empty">Nothing yet. Brief it before the call, or as it goes.</p>{/if}
        {/each}
      </div>

      <div class="group">
        <div class="head">
          <span class="label">Voices</span>
          <span class="mono faint">{a.speaker_id ? "told apart by voice" : "off (Setup)"}</span>
        </div>
        {#each voices as v (v.id)}
          <div class="voice" transition:slide={{ duration: 120 }}>
            <span class="vid mono">#{v.id}</span>
            <input
              class="vname"
              value={v.name}
              placeholder={`Speaker ${v.id}`}
              onkeydown={(e) => e.key === "Enter" && e.currentTarget.blur()}
              onchange={(e) => renameVoice(v, e.currentTarget.value)}
            />
            <span class="mono faint">{secs(v.seconds)}</span>
            <button class="x mono" title="Forget this voice" onclick={() => forgetVoice(v)}>×</button>
          </div>
        {:else}
          <p class="muted empty">Everyone it hears gets a number. Name them here, or tell it privately: “name the last speaker Ben” (or “name speaker 2 Ben”).</p>
        {/each}
      </div>

      <div class="group">
        <span class="label">About you</span>
        {#each about as it (it.key + it.t)}
          <div class="fact" transition:slide={{ duration: 120 }}>
            <span class="kind mono">{KIND[it.key]}</span>
            <span class="t">{it.t}</span>
            <button class="x mono" title="Forget" onclick={() => run("agent.context.remove", { section: it.key, text: it.t })}>×</button>
          </div>
        {:else}
          <p class="muted empty">Kept across conversations: “I'm a backend developer at Acme”, “I talk casually, no corporate speak”.</p>
        {/each}
      </div>
    </div>
  </div>
{:else}
  <div class="cols" in:fade={{ duration: 120 }}>
    <div class="col">
      <div class="group">
        <span class="label">Status</span>
        {#if a.enabled}
          {@const b = svc(st?.llm)}
          {@const v = svc(st?.tts)}
          {@const r = dev(st?.route, a.route.output ? "Opening…" : "Not set")}
          <dl class="readout">
            <dt class="label">Brain</dt>
            <dd class="mono {b.tone}">{b.text}</dd>
            <dt class="label">Voice</dt>
            <dd class="mono {v.tone}">{v.text}</dd>
            <dt class="label">Your voice</dt>
            <dd class="mono {(st?.voice_s ?? 0) >= 4 ? 'ok' : 'busy'}">{(st?.voice_s ?? 0).toFixed(1)} s{st?.voice_pinned ? " · kept" : (st?.voice_s ?? 0) < 4 ? " · say a few sentences" : ""}</dd>
            {#each st?.hear ?? [] as h (h.label)}
              {@const d = dev(h, "Waiting…")}
              <dt class="label">Hears {h.label}</dt>
              <dd class="mono {d.tone}">{d.text}</dd>
            {/each}
            <dt class="label">Virtual mic</dt>
            <dd class="mono {r.tone}">{r.text}</dd>
          </dl>
          {#if st?.llm.phase === "error" || st?.tts.phase === "error"}
            <button class="btn sm retry" onclick={() => run("agent.load")}>Retry loading</button>
          {/if}
        {:else}
          <p class="muted hint">The agent is off. Turn it on in Live.</p>
        {/if}
      </div>

      <div class="group">
        <span class="label">Ears</span>
        <p class="muted hint">What it hears, each labelled. One app (Discord) hears just the call.</p>
        <div class="chips">
          {#each a.hear.sources as s (s)}
            <span class="chip mono"
              >{sourceLabel(s)}<button
                class="x"
                title="Stop hearing"
                onclick={() =>
                  setCfg(
                    "agent.hear.sources",
                    a.hear.sources.filter((x) => x !== s),
                  )}>×</button
              ></span
            >
          {/each}
        </div>
        <Select value="" options={addableSources} width="100%" placeholder="Add an app or output…" onchange={(v) => v && setCfg("agent.hear.sources", [...a.hear.sources, v])} />
      </div>

      <div class="group">
        <span class="label">Brain</span>
        <Setting title="Model" stack>
          <Segmented
            value={a.llm.provider}
            options={[
              { value: "local", label: "Local (Gemma 4 E4B)" },
              { value: "custom", label: "Endpoint" },
            ]}
            onchange={(v) => setCfg("agent.llm.provider", v)}
          />
        </Setting>
        {#if a.llm.provider === "custom"}
          <div transition:slide={{ duration: 140 }}>
            <label class="field">
              <span class="label">Server address</span>
              <input class="input mono" value={a.llm.base_url} placeholder="https://api.openai.com/v1" onchange={(e) => setCfg("agent.llm.base_url", e.currentTarget.value.trim())} />
            </label>
            <div class="two">
              <label class="field">
                <span class="label">API key</span>
                <div class="keybox">
                  <input class="input mono" type={showKey ? "text" : "password"} value={a.llm.api_key} placeholder="empty for local" onchange={(e) => setCfg("agent.llm.api_key", e.currentTarget.value.trim())} />
                  <button class="eye label" onclick={() => (showKey = !showKey)}>{showKey ? "hide" : "show"}</button>
                </div>
              </label>
              <label class="field">
                <span class="label">Model name</span>
                <input class="input mono" value={a.llm.model} placeholder="gpt-4.1-mini" onchange={(e) => setCfg("agent.llm.model", e.currentTarget.value.trim())} />
              </label>
            </div>
          </div>
        {/if}
        <Setting title="Answers up to" hint="Sentences per answer. Speech should be short.">
          <Slider value={a.max_sentences} min={1} max={5} step={1} unit="" width={150} onchange={(v) => setCfg("agent.max_sentences", v)} />
        </Setting>
      </div>
    </div>

    <div class="col">
      <div class="group">
        <span class="label">Voice</span>
        <Setting title="Talk into" hint="An output that feeds a virtual microphone; pick its other end as the mic in Discord." stack>
          <Select value={a.route.output} options={routeOptions} width="100%" onchange={(v) => setCfg("agent.route.output", v)} />
        </Setting>
        <Setting title="Pass my mic through" hint="Your real voice goes to the virtual mic while the agent is quiet. Not needed (and skipped) when your mic already feeds it, as with Sonar.">
          <Toggle checked={a.route.passthrough} onchange={(v) => setCfg("agent.route.passthrough", v)} />
        </Setting>
        <Setting title="Let me hear it" hint="Play the agent (and sounds) where you listen.">
          <Toggle checked={a.route.monitor} onchange={(v) => setCfg("agent.route.monitor", v)} />
        </Setting>
        {#if a.route.monitor}
          <div transition:slide={{ duration: 120 }}>
            <Setting title="On" stack>
              <Select value={a.route.monitor_device} options={monitorOptions} width="100%" onchange={(v) => setCfg("agent.route.monitor_device", v)} />
            </Setting>
          </div>
        {/if}
        <Setting title="Voice model on the GPU" hint="Expert layers in VRAM, the rest read from RAM. 16/40: ~11 GB, first sound ~0.5 s. All: ~21 GB, ~0.1 s.">
          <Slider value={a.tts.vram_layers || 40} min={8} max={40} step={8} format={vram} width={150} onchange={(v) => setCfg("agent.tts.vram_layers", v >= 40 ? 0 : v)} />
        </Setting>
        <Setting title="Voice sample" hint="Seconds of your latest speech it clones from.">
          <Slider value={a.tts.voice_s} min={4} max={30} step={1} unit=" s" width={150} onchange={(v) => setCfg("agent.tts.voice_s", v)} />
        </Setting>
        <Setting title="Keep this voice" hint="Stop following your latest speech. Saved on this PC.">
          <Toggle checked={a.tts.pinned} onchange={(v) => setCfg("agent.tts.pinned", v)} />
        </Setting>
      </div>

      <div class="group">
        <span class="label">Manners</span>
        <Setting title="Forget the conversation after" hint="Of quiet. What it knows about you stays.">
          <Slider value={a.forget_after_min} min={0} max={120} step={5} format={mins} width={150} onchange={(v) => setCfg("agent.forget_after_min", v)} />
        </Setting>
        <Setting title="Wake word" hint="Start with it to talk to the agent privately. Empty turns it off.">
          <input class="input mono word" value={a.wakeword} placeholder="mimic" onchange={(e) => setCfg("agent.wakeword", e.currentTarget.value.trim())} />
        </Setting>
        <Setting title="Hear when they're done" hint="A small model listens to how they speak and answers as soon as they've finished, instead of waiting out a pause.">
          <Toggle checked={a.turn_detection} onchange={(v) => setCfg("agent.turn_detection", v)} />
        </Setting>
        <Setting title="Answer after" hint={a.turn_detection ? "Of quiet, when it can't tell whether they're done." : "How long the others must be quiet first."}>
          <Slider value={a.reply_after_ms} min={200} max={2000} step={50} unit=" ms" width={150} onchange={(v) => setCfg("agent.reply_after_ms", v)} />
        </Setting>
        <Setting title="When someone talks over it" hint="Finish: completes the sentence, then yields (a quick “mhm” doesn't count). The transcript keeps what it actually said." stack>
          <Segmented
            value={a.barge_in}
            options={[
              { value: "finish", label: "Finish the sentence" },
              { value: "stop", label: "Stop at once" },
              { value: "off", label: "Keep talking" },
            ]}
            onchange={(v) => setCfg("agent.barge_in", v)}
          />
        </Setting>
        <Setting title="Tell voices apart" hint="Labels each voice on the call (Speaker 1, 2, …) and remembers them by name. On this PC only.">
          <Toggle checked={a.speaker_id} onchange={(v) => setCfg("agent.speaker_id", v)} />
        </Setting>
        <Setting title="Mute me in autopilot" hint="Everything you say then goes to the agent, not the call.">
          <Toggle checked={a.mute_mic_in_autopilot} onchange={(v) => setCfg("agent.mute_mic_in_autopilot", v)} />
        </Setting>
      </div>
    </div>
  </div>
{/if}

<style>
  .live .col:first-child {
    display: flex;
    flex-direction: column;
  }
  .grow {
    flex: 1;
    min-height: 220px;
    display: flex;
    flex-direction: column;
  }
  .head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    padding-bottom: 8px;
    border-bottom: 1px solid var(--rule);
  }
  .head .label {
    padding: 0;
    border: 0;
  }
  .faint {
    font-size: 10.5px;
    color: var(--text-3);
  }
  .link {
    background: none;
    border: 0;
    padding: 0;
    font-size: 11px;
    color: var(--text-2);
    cursor: pointer;
  }
  .link:hover {
    color: var(--gold);
  }
  .now {
    display: flex;
    gap: 14px;
    align-items: stretch;
    margin-top: 14px;
  }
  .pilot {
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: flex-start;
    min-width: 200px;
    padding: 12px 14px;
    background: var(--void);
    border: 1px solid var(--rule);
    color: var(--text-2);
    cursor: pointer;
    clip-path: polygon(8px 0, 100% 0, 100% calc(100% - 8px), calc(100% - 8px) 100%, 0 100%, 0 8px);
    transition:
      border-color 90ms,
      color 90ms;
  }
  .pilot:hover {
    border-color: var(--rule-strong);
  }
  .pilot.on {
    border-color: var(--gold);
    color: var(--gold);
    background: color-mix(in srgb, var(--gold) 8%, transparent);
  }
  .pilot .state {
    font-family: "Licht Serif", "Fraunces", Georgia, serif;
    font-size: 20px;
    font-weight: 300;
    color: inherit;
  }
  .acts {
    display: flex;
    flex-direction: column;
    gap: 8px;
    justify-content: center;
  }
  .activity {
    font-size: 11px;
    color: var(--text-3);
    letter-spacing: 0.04em;
  }
  .log {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .line {
    display: grid;
    grid-template-columns: 104px 1fr auto;
    gap: 10px;
    padding: 7px 0;
    border-bottom: 1px solid var(--rule);
    font-size: 13px;
    line-height: 1.45;
  }
  .who {
    font-size: 10px;
    letter-spacing: 0.1em;
    color: var(--text-3);
    padding-top: 2px;
  }
  .src {
    letter-spacing: 0.02em;
  }
  .line.agent .who,
  .line.agent .text {
    color: var(--gold);
  }
  .line.private .text,
  .line.note .text {
    color: var(--text-2);
    font-style: italic;
  }
  .at {
    font-size: 10px;
    color: var(--text-3);
    padding-top: 2px;
  }
  .cut {
    font-size: 10px;
    color: var(--danger);
  }
  .modes {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 8px;
    margin-top: 12px;
  }
  .mode {
    display: flex;
    flex-direction: column;
    gap: 4px;
    align-items: flex-start;
    text-align: left;
    padding: 10px 12px;
    background: var(--void);
    border: 1px solid var(--rule);
    color: var(--text-2);
    cursor: pointer;
    transition:
      border-color 90ms,
      color 90ms;
  }
  .mode:hover {
    border-color: var(--rule-strong);
  }
  .mode.on {
    border-color: var(--gold);
    color: var(--gold);
    background: color-mix(in srgb, var(--gold) 8%, transparent);
  }
  .mname {
    font-family: "Licht Serif", "Fraunces", Georgia, serif;
    font-size: 16px;
    font-weight: 300;
  }
  .mblurb {
    font-size: 11px;
    line-height: 1.35;
    color: var(--text-3);
  }
  textarea.scenario {
    margin-top: 10px;
  }
  .told {
    margin-top: 10px;
  }
  .tline {
    display: grid;
    grid-template-columns: 104px 1fr auto;
    gap: 10px;
    padding: 5px 0;
    font-size: 12.5px;
    color: var(--text-2);
    font-style: italic;
  }
  .tline .who {
    font-style: normal;
  }
  .tline.note .who {
    color: var(--gold-dim);
  }
  .line.named .who {
    color: var(--text);
    letter-spacing: 0.04em;
  }
  .voice {
    display: grid;
    grid-template-columns: 34px 1fr auto auto;
    gap: 10px;
    align-items: center;
    padding: 4px 0;
    border-bottom: 1px solid var(--rule);
  }
  .vid {
    font-size: 10.5px;
    color: var(--text-3);
  }
  .vname {
    background: transparent;
    border: 1px solid transparent;
    color: var(--text);
    font: inherit;
    font-size: 13px;
    padding: 4px 6px;
    margin-left: -6px;
  }
  .vname:hover {
    border-color: var(--rule);
  }
  .vname:focus {
    outline: none;
    border-color: var(--gold-dim);
    background: var(--void);
  }
  .vname::placeholder {
    color: var(--text-2);
  }
  .brief {
    width: 100%;
    margin-top: 12px;
    resize: none;
    background: var(--void);
    border: 1px solid var(--gold-dim);
    color: var(--text);
    padding: 12px 14px;
    font: inherit;
    font-size: 14px;
    line-height: 1.45;
  }
  .brief:focus {
    outline: none;
    border-color: var(--gold);
  }
  .brief::placeholder {
    color: var(--text-3);
  }
  .summary {
    margin: 12px 0 4px;
    font-size: 13px;
    line-height: 1.5;
    color: var(--text-2);
    font-style: italic;
  }
  .fact {
    display: grid;
    grid-template-columns: 74px 1fr auto;
    gap: 10px;
    align-items: baseline;
    padding: 8px 0;
    border-bottom: 1px solid var(--rule);
    font-size: 13px;
  }
  .kind {
    font-size: 10px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--gold-dim);
  }
  .x {
    background: none;
    border: 0;
    color: var(--text-3);
    cursor: pointer;
    font-size: 14px;
  }
  .x:hover {
    color: var(--danger);
  }
  .empty,
  .hint {
    margin-top: 10px;
    font-size: 12px;
  }
  .readout {
    display: grid;
    grid-template-columns: 120px 1fr;
    gap: 8px 14px;
    margin: 14px 0 0;
  }
  dt {
    margin: 0;
  }
  dd {
    margin: 0;
    font-size: 11.5px;
    overflow-wrap: anywhere;
  }
  .ok {
    color: var(--text);
  }
  .busy {
    color: var(--gold);
  }
  .bad {
    color: var(--danger);
  }
  .dim {
    color: var(--text-3);
  }
  .retry {
    margin-top: 12px;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin: 12px 0;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 4px 4px 4px 10px;
    border: 1px solid var(--gold-dim);
    font-size: 11.5px;
    color: var(--text);
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-top: 12px;
  }
  .two {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
  }
  .keybox {
    position: relative;
  }
  .keybox .input {
    padding-right: 48px;
  }
  .input {
    width: 100%;
    font-size: 12.5px;
  }
  .eye {
    position: absolute;
    right: 1px;
    top: 1px;
    height: 36px;
    padding: 0 10px;
    border: 0;
    background: transparent;
  }
  .eye:hover {
    color: var(--gold);
  }
  textarea:not(.brief) {
    width: 100%;
    resize: vertical;
    background: var(--void);
    border: 1px solid var(--rule);
    color: var(--text);
    padding: 10px 12px;
    font: inherit;
    font-size: 13px;
    line-height: 1.45;
  }
  textarea:not(.brief):focus {
    outline: none;
    border-color: var(--gold-dim);
  }
  .word {
    width: 140px;
  }
</style>
