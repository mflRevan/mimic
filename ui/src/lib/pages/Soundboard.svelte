<script lang="ts">
  import { call } from "../backend";
  import HotkeyRecorder from "../components/HotkeyRecorder.svelte";
  import PageHeader from "../components/PageHeader.svelte";
  import Setting from "../components/Setting.svelte";
  import Slider from "../components/Slider.svelte";
  import Toggle from "../components/Toggle.svelte";
  import { app, setCfg, toast } from "../state.svelte";

  const c = $derived(app.config!);
  const sb = $derived(c.soundboard);
  let busy = $state<number | null>(null);

  async function run(method: string, params: Record<string, unknown> = {}) {
    try {
      return await call(method, params);
    } catch (e) {
      toast(String(e instanceof Error ? e.message : e), "error");
    }
  }

  /** Copy a picked file into Mimic's sounds folder (the slot keeps working if the original moves). */
  async function pick(slot: number, e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = "";
    if (!file) return;
    if (file.size > 25 * 1024 * 1024) {
      toast("That file is over 25 MB; pick a shorter sound.", "error");
      return;
    }
    busy = slot;
    const bytes = new Uint8Array(await file.arrayBuffer());
    let bin = "";
    for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
    await run("soundboard.import", { slot, name: file.name, data: btoa(bin) });
    busy = null;
  }

  const fileName = (p: string) => p.split(/[\\/]/).pop() ?? p;
</script>

<PageHeader index="03" title="Soundboard" subtitle="Nine sounds on shortcuts, played into your virtual mic so the call hears them, and on your own output. Press a shortcut again to stop it." />

<div class="cols">
  <div class="col wide">
    <div class="group">
      <span class="label">Slots</span>
      <div class="slots">
        {#each sb.slots as slot, i (i)}
          <div class="slot" class:empty={!slot.file}>
            <span class="num mono">{String(i + 1).padStart(2, "0")}</span>
            <div class="main">
              <input class="input name" value={slot.label} placeholder={slot.file ? fileName(slot.file) : "Empty"} onchange={(e) => setCfg(`soundboard.slots.${i}.label`, e.currentTarget.value.trim())} />
              <span class="file mono">{slot.file ? fileName(slot.file) : "no sound yet"}</span>
            </div>
            <HotkeyRecorder path={`hotkeys.sounds.${i}`} value={c.hotkeys.sounds[i] ?? ""} />
            <Slider value={Math.round(slot.volume * 100)} min={0} max={150} step={5} unit="%" width={110} onchange={(v) => setCfg(`soundboard.slots.${i}.volume`, v / 100)} />
            <label class="btn sm pickbtn">
              {busy === i + 1 ? "Adding…" : slot.file ? "Replace" : "Choose"}
              <input type="file" accept="audio/*,.wav,.mp3,.ogg,.flac" onchange={(e) => pick(i + 1, e)} hidden />
            </label>
            <button class="btn sm" disabled={!slot.file} onclick={() => run("soundboard.play", { slot: i + 1 })}>Play</button>
          </div>
        {/each}
      </div>
    </div>
  </div>

  <div class="col">
    <div class="group">
      <span class="label">Playback</span>
      <Setting title="Soundboard on" hint="Off frees the shortcuts.">
        <Toggle checked={sb.enabled} onchange={(v) => setCfg("soundboard.enabled", v)} />
      </Setting>
      <Setting title="Volume" hint="For all sounds; each slot has its own on top.">
        <Slider value={Math.round(sb.volume * 100)} min={0} max={100} step={5} unit="%" width={150} onchange={(v) => setCfg("soundboard.volume", v / 100)} />
      </Setting>
      <Setting title="Let me hear them" hint="Also play on your own output (set on the Agent page).">
        <Toggle checked={sb.monitor} onchange={(v) => setCfg("soundboard.monitor", v)} />
      </Setting>
      <button class="btn sm stop" onclick={() => run("soundboard.stop")}>Stop all sounds</button>
      <p class="muted hint">The call hears them through the virtual mic chosen on the Agent page (“Talk into”). WAV, MP3, OGG and FLAC, up to a minute.</p>
    </div>
  </div>
</div>

<style>
  .cols {
    grid-template-columns: 2fr 1fr;
  }
  .slots {
    margin-top: 12px;
    border-top: 1px solid var(--rule);
  }
  .slot {
    display: grid;
    grid-template-columns: 28px 1fr auto auto auto auto;
    gap: 12px;
    align-items: center;
    padding: 10px 0;
    border-bottom: 1px solid var(--rule);
  }
  .slot.empty .num {
    color: var(--text-3);
  }
  .num {
    font-size: 12px;
    color: var(--gold);
  }
  .main {
    display: flex;
    flex-direction: column;
    gap: 3px;
    min-width: 0;
  }
  .name {
    font-size: 13px;
    padding: 4px 8px;
  }
  .file {
    font-size: 10.5px;
    color: var(--text-3);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pickbtn {
    cursor: pointer;
  }
  .stop {
    margin-top: 14px;
  }
  .hint {
    margin-top: 12px;
    font-size: 12px;
  }
</style>
