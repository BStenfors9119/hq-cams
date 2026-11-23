// hqcam web UI — plain JS, talks to the JSON API on the same server.
"use strict";

const $ = (sel) => document.querySelector(sel);
const PHOTOS_SHOWN = 12;
let showAllPhotos = false;
let last = null; // the most recent /api/status

// ---- helpers ---------------------------------------------------------------

async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: body ? { "Content-Type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  if (res.status === 204) return null;
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw Object.assign(new Error(data.error || `${res.status} ${res.statusText}`), { status: res.status });
  return data;
}

// Download-only picture: the camera sends the JPEG straight back and keeps no
// copy, so hand it to the browser as a file download.
async function downloadPic() {
  const res = await fetch("/api/snapshots/download", { method: "POST" });
  if (!res.ok) {
    const data = await res.json().catch(() => ({}));
    throw new Error(data.error || `${res.status} ${res.statusText}`);
  }
  const blob = await res.blob();
  const match = (res.headers.get("Content-Disposition") || "").match(/filename="([^"]+)"/);
  const url = URL.createObjectURL(blob);
  const a = el("a", { href: url, download: match ? match[1] : "hqcam.jpg" });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 30000);
}

let toastTimer;
function toast(message, bad = false) {
  const t = $("#toast");
  t.textContent = message;
  t.className = bad ? "toast bad" : "toast";
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), bad ? 6000 : 3000);
}

function el(tag, props = {}, ...children) {
  const e = document.createElement(tag);
  Object.assign(e, props);
  for (const c of children) e.append(c);
  return e;
}

function duration(secs) {
  const d = Math.floor(secs / 86400), h = Math.floor(secs / 3600) % 24, m = Math.floor(secs / 60) % 60;
  return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : `${m}m`;
}

function when(unixSecs) {
  return new Date(unixSecs * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
}

function tempClass(c) {
  return c == null ? "" : c < 60 ? "ok" : c < 70 ? "warn" : "bad";
}

// ---- status ----------------------------------------------------------------

async function refreshStatus() {
  try {
    last = await api("GET", "/api/status");
  } catch {
    $("#live").textContent = "offline";
    $("#live").className = "pill bad";
    return;
  }
  const { health: h, camera: c, recorder: r, snapshots: s } = last;

  document.title = `${h.hostname} · hqcam`;
  $("#name").textContent = h.hostname || "hqcam";

  const alive = c.lastFrameMs != null && c.lastFrameMs < 3000;
  $("#live").textContent = alive ? "live" : "camera not sending";
  $("#live").className = `pill ${alive ? "ok" : "bad"}`;
  $("#temp").textContent = h.tempC != null ? `${h.tempC.toFixed(1)} °C` : "";
  $("#temp").className = `pill ${tempClass(h.tempC)}`;

  renderStatus(h, c, r, s);
  renderStorage(s.storage);
  renderRecord(r);
  renderSettings(c);
  if (s.count !== photoCount) refreshPhotos();
}

function renderStatus(h, c, r, s) {
  const rows = [];
  const add = (label, value, cls = "") => rows.push(el("dt", { textContent: label }), el("dd", { textContent: value, className: cls }));

  if (h.tempC != null) add("Temperature", `${h.tempC.toFixed(1)} °C`, tempClass(h.tempC));
  if (h.throttle) {
    const t = h.throttle;
    let msg = "Running at full speed", cls = "ok";
    if (t.underVoltageNow) [msg, cls] = ["Power supply too weak right now", "bad"];
    else if (t.throttledNow || t.freqCappedNow) [msg, cls] = ["Slowed down right now (too hot)", "bad"];
    else if (t.underVoltageSinceBoot) [msg, cls] = ["Power dipped since start-up", "warn"];
    else if (t.throttledSinceBoot || t.freqCappedSinceBoot) [msg, cls] = ["Was slowed down by heat since start-up", "warn"];
    add("Speed", msg, cls);
  }
  if (h.load) {
    const pct = Math.round((h.load[0] / h.cpus) * 100);
    add("CPU", `${pct}% busy`, pct > 75 ? "warn" : "");
  }
  if (h.memTotalMb) add("Memory", `${h.memAvailableMb} MB free of ${h.memTotalMb} MB`);
  if (h.wifi) add("WiFi", `${h.wifi.ssid} · signal ${h.wifi.signal}%`, h.wifi.signal < 40 ? "warn" : "");
  else add("WiFi", "not connected (or hotspot mode)", "warn");
  if (h.uptimeSecs != null) add("Up for", duration(h.uptimeSecs));
  add("Picture", `${c.width}×${c.height} at ${c.framerate} fps`);
  add("Pictures", s.storage
    ? `${s.count} saved on the ${storageName(s.storage)}${s.freeMb != null ? ` · ${(s.freeMb / 1024).toFixed(1)} GB free` : ""}`
    : "download only (no drive connected)", s.storage ? "" : "warn");
  add("USB drive", r.driveMounted ? `plugged in · ${r.freeMb != null ? (r.freeMb / 1024).toFixed(1) + " GB free" : ""}` : "not plugged in");
  add("Software", `hqcam ${h.version}`);

  $("#status").replaceChildren(...rows);
}

function storageName(st) {
  return st.kind === "network" ? "network drive" : "USB drive";
}

function renderStorage(st) {
  $("#snap").textContent = st ? "Take a Pic" : "Take a Pic & download";
  $("#snap-note").textContent = st
    ? `Pictures are saved to the ${storageName(st)}.`
    : "No USB or network drive is connected, so the camera can't keep pictures. Each one downloads straight to this device instead.";
  $("#photos-section").hidden = !st;
}

function renderRecord(r) {
  const b = $("#record");
  b.textContent = r.manual ? "Stop recording" : "Record";
  b.className = r.manual ? "recording" : "";
  b.disabled = !r.manual && !r.driveMounted;
  let note = "";
  if (!r.driveMounted) note = "Recording needs a USB drive labelled HQCAM plugged into the camera.";
  else if (r.recording && !r.manual) note = "Recording automatically (away from HQ WiFi).";
  else if (r.recording) note = "Recording to the USB drive…";
  $("#record-note").textContent = note;
}

function renderSettings(c) {
  for (const b of document.querySelectorAll("#rotation button")) {
    b.classList.toggle("active", Number(b.dataset.rotation) === c.orientation.rotation);
  }
  $("#hflip").checked = c.orientation.hflip;
  $("#vflip").checked = c.orientation.vflip;
  const lp = c.focus.lensPosition;
  const which = lp == null ? "auto" : lp === "0" ? "0" : "custom";
  for (const b of document.querySelectorAll("#focus button")) b.classList.toggle("active", b.dataset.focus === which);
}

// ---- pictures --------------------------------------------------------------

let photoCount = -1;

async function refreshPhotos() {
  let list;
  try {
    list = await api("GET", "/api/snapshots");
  } catch (e) {
    return toast(`Couldn't load pictures: ${e.message}`, true);
  }
  photoCount = list.length;
  $("#photo-count").textContent = list.length ? `${list.length} saved` : "";
  const shown = showAllPhotos ? list : list.slice(0, PHOTOS_SHOWN);
  $("#more").hidden = showAllPhotos || list.length <= PHOTOS_SHOWN;
  if (!list.length) {
    $("#photos").replaceChildren(el("p", { className: "note", textContent: "No pictures yet — tap Take a Pic." }));
    return;
  }
  $("#photos").replaceChildren(...shown.map(photoCard));
}

function photoCard(p) {
  const img = el("img", { src: p.url, alt: `Picture taken ${when(p.taken)}`, loading: "lazy" });
  const del = el("button", { textContent: "Delete", title: "Delete this picture" });
  del.addEventListener("click", async () => {
    if (!confirm("Delete this picture?")) return;
    try {
      await api("DELETE", `/api/snapshots/${p.date}/${p.name}`);
      refreshPhotos();
    } catch (e) {
      toast(`Couldn't delete: ${e.message}`, true);
    }
  });
  return el("div", { className: "photo" },
    el("a", { href: p.url, target: "_blank", rel: "noopener" }, img),
    el("div", { className: "meta" },
      el("span", { textContent: when(p.taken) }),
      el("a", { href: `${p.url}?download=1`, textContent: "Save" }),
      del));
}

// ---- actions ---------------------------------------------------------------

$("#snap").addEventListener("click", async (ev) => {
  const b = ev.currentTarget;
  b.disabled = true;
  try {
    if (last && last.snapshots.storage) {
      try {
        await api("POST", "/api/snapshots");
        toast("Picture saved");
        refreshPhotos();
        return;
      } catch (e) {
        if (e.status !== 409) throw e; // 409: the drive went away since the last check
      }
    }
    await downloadPic();
    toast("Picture downloaded to this device");
    refreshStatus();
  } catch (e) {
    toast(`No picture taken: ${e.message}`, true);
  } finally {
    b.disabled = false;
  }
});

$("#record").addEventListener("click", async () => {
  const on = !(last && last.recorder.manual);
  try {
    renderRecord(await api("POST", "/api/record", { on }));
    toast(on ? "Recording started" : "Recording stopped");
  } catch (e) {
    toast(`Couldn't ${on ? "start" : "stop"} recording: ${e.message}`, true);
  }
});

$("#more").addEventListener("click", () => {
  showAllPhotos = true;
  refreshPhotos();
});

async function setOrientation(change) {
  try {
    await api("POST", "/api/orientation", change);
    toast("Camera turned — the picture will blink for a second");
    refreshStatus();
  } catch (e) {
    toast(`Couldn't change it: ${e.message}`, true);
  }
}

for (const b of document.querySelectorAll("#rotation button")) {
  b.addEventListener("click", () => setOrientation({ rotation: Number(b.dataset.rotation) }));
}
$("#hflip").addEventListener("change", (e) => setOrientation({ hflip: e.target.checked }));
$("#vflip").addEventListener("change", (e) => setOrientation({ vflip: e.target.checked }));

async function setFocus(lensPosition) {
  try {
    await api("POST", "/api/focus", { lensPosition });
    toast(lensPosition == null ? "Autofocus on" : "Focus set");
    refreshStatus();
  } catch (e) {
    toast(`Couldn't set focus: ${e.message}`, true);
  }
}

for (const b of document.querySelectorAll("#focus button")) {
  b.addEventListener("click", () => {
    const f = b.dataset.focus;
    $("#focus-custom").hidden = f !== "custom";
    if (f === "auto") setFocus(null);
    else if (f === "0") setFocus(0);
  });
}
$("#focus-apply").addEventListener("click", () => {
  const metres = Number($("#focus-metres").value);
  if (!(metres > 0)) return toast("Enter a distance in metres", true);
  // The lens takes dioptres: 1 / distance in metres.
  setFocus(Math.round((1 / metres) * 100) / 100);
});

// If the stream drops (camera restart, WiFi blip), reconnect it.
$("#stream").addEventListener("error", () => {
  setTimeout(() => ($("#stream").src = `/stream.mjpg?t=${Date.now()}`), 2000);
});

refreshStatus();
setInterval(refreshStatus, 5000);
