/* Clipping Factory studio — one screen, three states, SSE-driven. */
(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const STAGE_LABELS = {
    inspecting: "1. Inspect",
    extracting_audio: "2. Extract audio",
    transcribing: "3. Transcribe",
    selecting_candidates: "4. Find moments",
    validating_candidates: "5. Validate",
    analyzing_layout: "6. Analyze framing",
    rendering: "7. Render",
  };
  const STAGE_ORDER = Object.keys(STAGE_LABELS);

  let projectId = localStorage.getItem("cf-project") || null;
  let view = null;
  let sse = null;
  let refetchTimer = null;
  let elapsedTimer = null;
  let liveProgress = null; // {stage, progress, detail}
  let captionStyle = localStorage.getItem("cf-caption-style") || "impact";
  let accentColor = localStorage.getItem("cf-accent-color") || "#FFDD00";

  // ------------------------------------------------------------------ setup
  async function loadSetup() {
    try {
      const s = await (await fetch("/api/setup")).json();
      const problems = [];
      if (!s.ffmpeg) problems.push("FFmpeg was not found. Install it and restart.");
      else if (!s.ffmpeg_ass) problems.push("This FFmpeg build cannot burn captions. macOS: brew install ffmpeg-full, then restart.");
      if (!s.ffprobe) problems.push("FFprobe was not found. It ships with FFmpeg.");
      if (!s.whisper_ok) problems.push("whisper-cli was not found. macOS: brew install whisper-cpp, or set CF_WHISPER_BIN.");
      if (!s.model_ok) problems.push(`Transcription model missing (~148 MB). Download ggml-base.en.bin into ${s.data_dir}/models/`);
      if (s.disk_free_gb !== null && s.disk_free_gb < 2) problems.push(`Low disk space: ${s.disk_free_gb.toFixed(1)} GB free.`);
      const banner = $("setup-banner");
      if (problems.length) {
        banner.textContent = problems.join("\n");
        banner.classList.remove("hidden");
      } else {
        banner.classList.add("hidden");
      }
    } catch { /* server will complain loudly enough */ }
  }

  async function loadSettings() {
    try {
      const s = await (await fetch("/api/settings/ai")).json();
      const dot = $("ai-dot");
      dot.className = "dot";
      if (s.provider === "offline") { dot.classList.add("offline"); $("ai-label").textContent = "Local ranking"; }
      else if (s.connected) { dot.classList.add("on"); $("ai-label").textContent = `${s.provider} · ${s.model}`; }
      else { $("ai-label").textContent = "AI connection"; }
      $("provider").value = s.provider || "openai";
      $("model").value = s.model || "";
      syncModalRows();
    } catch { /* ignore */ }
  }

  // ------------------------------------------------------------------ upload
  function wireUpload() {
    const drop = $("drop");
    document.querySelectorAll(".seg-btn").forEach((b) => {
      if (b.dataset.style === captionStyle) {
        document.querySelectorAll(".seg-btn").forEach((x) => x.classList.remove("active"));
        b.classList.add("active");
      }
      b.addEventListener("click", () => {
        captionStyle = b.dataset.style;
        localStorage.setItem("cf-caption-style", captionStyle);
        document.querySelectorAll(".seg-btn").forEach((x) => x.classList.remove("active"));
        b.classList.add("active");
      });
    });
    const syncSwatches = () => {
      document.querySelectorAll(".swatch").forEach((s) =>
        s.classList.toggle("active", s.dataset.color.toUpperCase() === accentColor.toUpperCase())
      );
      const custom = $("custom-color");
      const isPreset = [...document.querySelectorAll(".swatch")].some(
        (s) => s.dataset.color.toUpperCase() === accentColor.toUpperCase()
      );
      custom.classList.toggle("active", !isPreset);
      custom.value = accentColor;
    };
    document.querySelectorAll(".swatch").forEach((s) =>
      s.addEventListener("click", () => {
        accentColor = s.dataset.color;
        localStorage.setItem("cf-accent-color", accentColor);
        syncSwatches();
      })
    );
    $("custom-color").addEventListener("input", (e) => {
      accentColor = e.target.value;
      localStorage.setItem("cf-accent-color", accentColor);
      syncSwatches();
    });
    syncSwatches();
    $("choose-btn").addEventListener("click", () => $("file-input").click());
    $("file-input").addEventListener("change", (e) => {
      if (e.target.files[0]) uploadFile(e.target.files[0]);
    });
    ["dragenter", "dragover"].forEach((ev) =>
      drop.addEventListener(ev, (e) => { e.preventDefault(); drop.classList.add("dragover"); })
    );
    ["dragleave", "drop"].forEach((ev) =>
      drop.addEventListener(ev, (e) => { e.preventDefault(); drop.classList.remove("dragover"); })
    );
    drop.addEventListener("drop", (e) => {
      const f = e.dataTransfer.files && e.dataTransfer.files[0];
      if (f) uploadFile(f);
    });
  }

  function uploadFile(file) {
    if (!/\.(mp4|m4v)$/i.test(file.name)) {
      alert("Attach an .mp4 file (the MVP accepts MP4 sources only).");
      return;
    }
    $("drop").classList.add("hidden");
    $("upload-progress").classList.remove("hidden");
    $("upload-label").textContent = `Uploading ${file.name}…`;

    const form = new FormData();
    form.append("caption_style", captionStyle);
    form.append("accent_color", accentColor);
    form.append("file", file, file.name);
    const xhr = new XMLHttpRequest();
    xhr.open("POST", "/api/projects");
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) $("upload-bar").style.width = `${(e.loaded / e.total) * 100}%`;
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        const v = JSON.parse(xhr.responseText);
        projectId = v.project.id;
        localStorage.setItem("cf-project", projectId);
        view = v;
        connectSse();
        render();
      } else {
        let msg = "Upload failed.";
        try { msg = JSON.parse(xhr.responseText).error || msg; } catch {}
        alert(msg);
        resetToEmpty();
      }
    };
    xhr.onerror = () => { alert("Upload failed — is the server still running?"); resetToEmpty(); };
    xhr.send(form);
  }

  function resetToEmpty() {
    projectId = null;
    view = null;
    liveProgress = null;
    localStorage.removeItem("cf-project");
    if (sse) { sse.close(); sse = null; }
    $("drop").classList.remove("hidden");
    $("upload-progress").classList.add("hidden");
    $("upload-bar").style.width = "0%";
    $("file-input").value = "";
    render();
  }

  // ------------------------------------------------------------------ data
  async function refetch() {
    if (!projectId) return;
    try {
      const res = await fetch(`/api/projects/${projectId}`);
      if (res.status === 404) { resetToEmpty(); return; }
      view = await res.json();
      render();
    } catch { /* transient */ }
  }

  function scheduleRefetch() {
    clearTimeout(refetchTimer);
    refetchTimer = setTimeout(refetch, 180);
  }

  function connectSse() {
    if (sse) sse.close();
    if (!projectId) return;
    sse = new EventSource(`/api/projects/${projectId}/events`);
    sse.onmessage = (e) => {
      let msg = {};
      try { msg = JSON.parse(e.data); } catch { return; }
      if (msg.type === "snapshot" && msg.view) { view = msg.view; render(); return; }
      if (msg.type === "progress") {
        liveProgress = { stage: msg.stage, progress: msg.progress, detail: msg.detail };
        renderLive();
        return;
      }
      // stage / clip / done → authoritative refetch
      liveProgress = null;
      scheduleRefetch();
    };
    sse.onerror = () => { /* EventSource auto-reconnects */ };
  }

  // ------------------------------------------------------------------ render
  function render() {
    const p = view && view.project;
    $("upload-state").classList.toggle("hidden", !!p);
    $("processing-state").classList.toggle("hidden", !p);
    if (!p) { $("results-state").classList.add("hidden"); stopElapsed(); return; }

    // Source line
    const src = p.source;
    $("source-name").textContent = view.original_name || "source.mp4";
    $("source-meta").textContent = src
      ? `${src.width}×${src.height} · ${fmtMs(src.duration_ms)} · ${src.video_codec}/${src.audio_codec}`
      : "";

    // Warning banner
    const warn = $("warning-banner");
    if (p.warning) { warn.textContent = p.warning; warn.classList.remove("hidden"); }
    else warn.classList.add("hidden");

    renderStages(p);
    renderCurrentOp(p);
    renderError(p);
    renderResults(p);
    startElapsed(p);
  }

  function stageState(p, name) {
    const rec = p.stages.find((s) => s.name === name) || {};
    if (rec.error) return "failed";
    if (rec.completed_at) return "done";
    if (rec.started_at) return "active";
    return "pending";
  }

  function renderStages(p) {
    const wrap = $("stages");
    wrap.innerHTML = "";
    for (const name of STAGE_ORDER) {
      const rec = p.stages.find((s) => s.name === name) || {};
      const st = stageState(p, name);
      const div = document.createElement("div");
      div.className = `step ${st === "pending" ? "" : st}`;
      div.dataset.stage = name;
      const status =
        st === "failed" ? "Failed" :
        st === "done" ? (rec.detail || "Done") :
        st === "active" ? (rec.detail || "Working…") : "";
      div.innerHTML = `<strong>${STAGE_LABELS[name]}</strong><span class="status"></span>` +
        (st === "active" ? `<div class="mini-bar"><div class="mini-fill"></div></div>` : "");
      div.querySelector(".status").textContent = status;
      wrap.appendChild(div);
    }
    renderLive();
  }

  function renderLive() {
    if (!liveProgress && view && view.live) liveProgress = view.live;
    if (!liveProgress) return;
    const step = document.querySelector(`.step[data-stage="${liveProgress.stage}"] .mini-fill`);
    if (step) step.style.width = `${Math.round((liveProgress.progress || 0) * 100)}%`;
    const status = document.querySelector(`.step[data-stage="${liveProgress.stage}"] .status`);
    if (status && liveProgress.detail) status.textContent = liveProgress.detail;
    if (liveProgress.detail) $("current-op-text").textContent = liveProgress.detail;
    else if (liveProgress.progress != null)
      $("current-op-text").textContent =
        `${STAGE_LABELS[liveProgress.stage] || liveProgress.stage} — ${Math.round(liveProgress.progress * 100)}%`;
  }

  function renderCurrentOp(p) {
    const active = ["inspecting","extracting_audio","transcribing","selecting_candidates","validating_candidates","analyzing_layout","rendering"].includes(p.status);
    $("current-op").classList.toggle("hidden", !active);
    $("cancel-btn").classList.toggle("hidden", !active);
    if (active) {
      const label = STAGE_LABELS[p.status] || p.status;
      $("current-op-text").textContent = label.replace(/^\d+\.\s*/, "") + "…";
    }
  }

  function renderError(p) {
    const box = $("error-box");
    if (p.status === "failed" && p.error) {
      const failedStage = p.stages.find((s) => s.error);
      $("error-stage").textContent = failedStage
        ? `${STAGE_LABELS[failedStage.name] || failedStage.name} failed`
        : "Processing failed";
      $("error-text").textContent = p.error;
      box.classList.remove("hidden");
    } else if (p.status === "cancelled") {
      $("error-stage").textContent = "Cancelled";
      $("error-text").textContent = "Processing was stopped. Completed clips are kept. Retry resumes from the last completed stage.";
      box.classList.remove("hidden");
    } else {
      box.classList.add("hidden");
    }
  }

  function renderResults(p) {
    const section = $("results-state");
    const clips = (view.clips || []);
    const ready = clips.filter((c) => c.status === "ready");
    const showResults = clips.length > 0 || p.status === "complete";
    section.classList.toggle("hidden", !showResults);
    if (!showResults) return;

    const total = clips.length;
    $("results-title").textContent =
      total === 0 ? "No clips produced" :
      p.status === "complete"
        ? `${ready.length} strong clip${ready.length === 1 ? "" : "s"} found`
        : `${ready.length} of ${total} clips ready`;

    const sel = view.selector ? ` Selected by ${view.selector}.` : "";
    $("results-sub").textContent =
      `Ranked by self-contained opening, tension, payoff, and clarity.${sel}`;

    // Empty (quality bar) state
    $("empty-results").classList.toggle("hidden", !(p.status === "complete" && total === 0));

    const wrap = $("clips");
    wrap.innerHTML = "";
    for (const c of clips) {
      wrap.appendChild(clipRow(c));
    }

    // Rejected transparency
    const rej = view.rejected_summary || [];
    $("rejected-details").classList.toggle("hidden", rej.length === 0);
    if (rej.length) {
      const list = $("rejected-list");
      list.innerHTML = "";
      for (const r of rej) {
        const d = document.createElement("div");
        d.className = "rejected-item";
        d.innerHTML = `<div></div><div class="reasons"></div>`;
        d.children[0].textContent = `“${r.headline || "(untitled)"}” — ${fmtMs(r.start_ms)}–${fmtMs(r.end_ms)}`;
        d.children[1].textContent = (r.reasons || []).join("; ");
        list.appendChild(d);
      }
    }
  }

  function clipRow(c) {
    const row = document.createElement("article");
    row.className = "clip";

    const preview = document.createElement("div");
    preview.className = "preview";
    if (c.status === "ready") {
      const v = document.createElement("video");
      v.controls = true;
      v.preload = "metadata";
      v.playsInline = true;
      v.src = `/api/projects/${projectId}/clips/${c.id}`;
      preview.appendChild(v);
    } else if (c.status === "rendering") {
      preview.innerHTML = `<span class="spinner"></span>`;
    } else if (c.status === "failed") {
      preview.textContent = "render failed";
    } else {
      preview.textContent = "queued";
    }

    const body = document.createElement("div");
    const rankLabel = c.rank === 1 ? "Best candidate" : `Candidate ${c.rank}`;
    const badges = [];
    if (c.layout && c.layout.mode === "face_crop") badges.push(`<span class="badge">face-tracked crop</span>`);
    else badges.push(`<span class="badge">blur-pad layout</span>`);
    if (c.low_confidence) badges.push(`<span class="badge warn">low transcription confidence</span>`);
    if (c.status === "failed") badges.push(`<span class="badge bad">failed</span>`);
    body.innerHTML = `
      <div class="rank"></div>
      <h3></h3>
      <p class="times"></p>
      <p class="why"></p>
      <div class="badges">${badges.join("")}</div>`;
    body.querySelector(".rank").textContent = `${rankLabel} · ${fmtMs(c.duration_ms)}`;
    body.querySelector("h3").textContent = `“${c.headline}”`;
    body.querySelector(".times").textContent =
      `Starts at ${fmtMs(c.start_ms)} and ends at ${fmtMs(c.end_ms)}. One continuous excerpt from the podcast.`;
    body.querySelector(".why").textContent = c.status === "failed" && c.error
      ? `Render error: ${c.error}`
      : `Why it works: ${c.selection_reason}`;

    const actions = document.createElement("div");
    actions.className = "actions";
    if (c.status === "ready") {
      const a = document.createElement("a");
      a.href = `/api/projects/${projectId}/clips/${c.id}/download`;
      a.innerHTML = `<button class="primary" type="button">Download MP4</button>`;
      actions.appendChild(a);
    } else if (c.status === "failed") {
      const b = document.createElement("button");
      b.textContent = "Retry failed clips";
      b.addEventListener("click", retry);
      actions.appendChild(b);
    }

    row.appendChild(preview);
    row.appendChild(body);
    row.appendChild(actions);
    return row;
  }

  // ------------------------------------------------------------------ elapsed
  function startElapsed(p) {
    stopElapsed();
    const activeStage = p.stages.find((s) => s.started_at && !s.completed_at && !s.error);
    if (!activeStage) { $("elapsed").textContent = ""; return; }
    const started = new Date(activeStage.started_at).getTime();
    elapsedTimer = setInterval(() => {
      const s = Math.max(0, Math.floor((Date.now() - started) / 1000));
      $("elapsed").textContent = `· ${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")} elapsed`;
    }, 1000);
  }
  function stopElapsed() { clearInterval(elapsedTimer); }

  // ------------------------------------------------------------------ actions
  async function cancel() {
    if (!projectId) return;
    await fetch(`/api/projects/${projectId}/cancel`, { method: "POST" });
  }
  async function retry() {
    if (!projectId) return;
    await fetch(`/api/projects/${projectId}/retry`, { method: "POST" });
    scheduleRefetch();
  }
  async function openFolder() {
    if (!projectId) return;
    const r = await (await fetch(`/api/projects/${projectId}/open-output-folder`, { method: "POST" })).json();
    if (!r.opened) alert(`Clips are saved to:\n${r.path}`);
  }

  // ------------------------------------------------------------------ modal
  function syncModalRows() {
    const offline = $("provider").value === "offline";
    $("key-row").classList.toggle("hidden", offline);
    $("model-row").classList.toggle("hidden", offline);
    $("offline-note").classList.toggle("hidden", !offline);
    $("model").placeholder = $("provider").value === "anthropic" ? "claude-sonnet-4-5" : "gpt-4o-mini";
  }

  function wireModal() {
    $("ai-btn").addEventListener("click", () => $("modal-backdrop").classList.remove("hidden"));
    $("modal-close").addEventListener("click", () => {
      $("modal-backdrop").classList.add("hidden");
      $("test-result").classList.add("hidden");
      $("api-key").value = "";
    });
    $("provider").addEventListener("change", syncModalRows);
    $("test-save").addEventListener("click", async () => {
      const btn = $("test-save");
      btn.disabled = true;
      btn.textContent = "Testing…";
      try {
        const res = await fetch("/api/settings/ai", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            provider: $("provider").value,
            model: $("model").value.trim(),
            api_key: $("api-key").value.trim(),
          }),
        });
        if (!res.ok) throw new Error((await res.json()).error || "Could not save settings.");
        const test = await (await fetch("/api/settings/ai/test", { method: "POST" })).json();
        const out = $("test-result");
        out.textContent = test.message;
        out.className = `small ${test.ok ? "ok" : "bad"}`;
        out.classList.remove("hidden");
        if (test.ok) $("api-key").value = "";
        loadSettings();
      } catch (err) {
        const out = $("test-result");
        out.textContent = err.message;
        out.className = "small bad";
        out.classList.remove("hidden");
      } finally {
        btn.disabled = false;
        btn.textContent = "Test & save";
      }
    });
  }

  // ------------------------------------------------------------------ misc
  function fmtMs(ms) {
    const t = Math.floor((ms || 0) / 1000);
    const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), s = t % 60;
    return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`
                 : `${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
  }

  // ------------------------------------------------------------------ boot
  function boot() {
    wireUpload();
    wireModal();
    $("cancel-btn").addEventListener("click", cancel);
    $("retry-btn").addEventListener("click", retry);
    $("open-folder-btn").addEventListener("click", openFolder);
    $("new-project-btn").addEventListener("click", resetToEmpty);
    loadSetup();
    loadSettings();
    if (projectId) {
      refetch().then(() => connectSse());
    }
  }
  boot();
})();
