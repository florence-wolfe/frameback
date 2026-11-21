# Frameback control API

Frameback runs a small local HTTP server so hardware controllers (a Stream Deck plugin), scripts and shortcuts can drive it. Everything you can do from the toolbar and keyboard is available, plus a status document to draw buttons from.

- **Transport:** HTTP/1.1, JSON bodies, on `127.0.0.1` only.
- **Default port:** `47800`. Set `FRAMEBACK_API_PORT=<port>` before launching to change it, or `FRAMEBACK_API_PORT=off` to turn the API off. If the port is taken (a second instance, another program) Frameback says so in an on-screen message and runs without the API.
- **Version prefix:** every path starts with `/v1/`.

```sh
curl localhost:47800/v1/status                       # read state
curl -X POST localhost:47800/v1/record -d '{"on":true}'   # start recording
```

## Conventions

### Requests

- `GET` reads state. `POST` changes it. Using the wrong method gives `405`.
- A `POST` body is a JSON object, or empty if the endpoint takes no fields. Unknown fields are ignored.
- Bodies are limited to 64 KiB. Chunked request bodies aren't supported (send `Content-Length`, as every normal client does).

### Switches

Endpoints that turn something on or off take an optional `{"on": true|false}`:

| Body | Meaning |
| --- | --- |
| `{"on": true}` | Make it on (does nothing if it already is) |
| `{"on": false}` | Make it off (does nothing if it already is) |
| `{}` or empty | Flip it |

Prefer explicit `on` values from a controller: a button that sends "make it on" can't get out of step with the app if it was changed from the keyboard.

Selecting a setting that's already selected (a codec, device, format…) is also a no-op that succeeds, even in situations where changing it would be refused.

### Responses

`GET /v1/status` and `GET /v1/options` return their document directly.

Every `POST` returns the **new status**, so you can redraw without a second request:

```json
{
  "ok": true,
  "status": { "...": "see Status" }
}
```

Commands that produce something add fields beside `ok` (`path`, `frames`; see each endpoint).

Errors have the same envelope with a message meant for people:

```json
{ "ok": false, "error": "Go LIVE (L) before changing codec: it would clear the rewind buffer" }
```

| Code | Meaning |
| --- | --- |
| `400` | Malformed request or a bad value (wrong type, not one of the allowed values) |
| `403` | Refused: the request came from a web page (has an `Origin` header) or isn't addressed to localhost |
| `404` | Unknown endpoint, or the thing you named doesn't exist (device id, file, codec…) |
| `405` | Wrong HTTP method for this endpoint |
| `409` | Understood, but not allowed in the current state: reviewing or importing (changing the setup would clear the buffer), or not in clip mode |
| `413` | Body too large |
| `500` | It was allowed but failed (writing a file, for example) |
| `503` | Nothing to act on yet: no capture device running, nothing buffered |
| `504` | Frameback didn't answer within 120 s |

Only `2xx` responses have `"ok": true`.

### Times

All positions and lengths are **seconds from the start of the timeline** (the oldest frame on the bar), as the timeline displays them. While reviewing, the timeline's span is frozen; going live unfreezes it.

## Status

`GET /v1/status`. Cheap to call; polling it once or twice a second is fine.

```json
{
  "version": "0.3.1",
  "mode": "live",
  "playing": false,
  "recording": { "seconds": 12.4 },
  "timeline": { "length": 58.2, "position": 58.2 },
  "clip": { "start": 10.0, "end": null },
  "file": { "name": "match.mp4", "downloading": false, "progress": 0.4, "ready": false },
  "capture": {
    "active": true,
    "waiting_for_device": false,
    "device": { "id": "0x2100000fd9006a", "name": "Game Capture HD60 S+" },
    "format": "720×480 @ 59.94 yuvs",
    "fps": 59.9,
    "latency_ms": 2.3
  },
  "audio": { "device": "Game Capture HD60 S+", "muted": false },
  "settings": { "codec": "hevc", "buffer_minutes": 1, "step_frames": 1, "aspect": "captured" },
  "view": { "fullscreen": false, "stats": false, "overlays_hidden": false },
  "message": null
}
```

| Field | Notes |
| --- | --- |
| `mode` | `live`, `review` (paused or playing the buffer), `clip` (clip mode, a sub-mode of review), or `import` (a file is downloading or converting; not yet reviewable) |
| `playing` | True while the buffer or file is playing in review. False when live |
| `recording` | `null` when not recording, otherwise how long it has been |
| `timeline` | `null` until something is buffered. `position` equals `length` when live |
| `clip` | `null` outside clip mode. `start`/`end` are the marked frames, each `null` until marked. With neither marked, saving takes the whole buffer or file |
| `file` | `null` unless a file is open or importing. `ready` is true once it can be reviewed; `progress` is 0–1 for the current phase (`downloading`, then converting) |
| `capture.active` | False while there's no running capture (no device, or unplugged). `waiting_for_device` is true after an unplug: capture resumes by itself when the device returns |
| `capture.device`, `capture.format` | `null` when none is selected. `format` is the label to pass to `/v1/format` |
| `capture.fps`, `capture.latency_ms` | Measured. `latency_ms` can be `null` |
| `audio.device` | The monitored input's name, or `null` when audio is off |
| `message` | The text of the on-screen toast, if one is showing (they disappear after 4 s) |

`view.fullscreen` can lag a request by a moment: window managers apply it asynchronously, so the `status` in a `/v1/fullscreen` response may still show the old value. The next poll is right.

## Options

`GET /v1/options` lists what each setting can be set to. Fetch it when a settings screen opens, not on every poll: devices and formats change when hardware is plugged in or capture restarts.

```json
{
  "video_devices": [{ "id": "…", "name": "Game Capture HD60 S+" }],
  "formats": [{ "label": "720×480 @ 59.94 yuvs", "width": 720, "height": 480, "fps": 59.94, "pixel": "yuvs" }],
  "audio_devices": [{ "name": "Game Capture HD60 S+" }],
  "codecs": [{ "id": "hevc", "label": "HEVC", "extension": "mp4" }],
  "buffer_minutes": [1, 5, 10, 30, 60],
  "step_frames": [1, 5, 10, 30, 60],
  "aspects": [{ "id": "captured", "label": "As captured" }]
}
```

`formats` are the modes of the **selected** video device, so it's empty until capture has started.

## Endpoints

### Recording and capture

| Endpoint | Body | Does |
| --- | --- | --- |
| `POST /v1/record` | switch | Start or stop recording to `~/Movies` (`~/Videos` on Windows/Linux). Starting needs live mode: `409` while reviewing or with a file open, `503` before capture is running. Stopping always works. |
| `POST /v1/screenshot` | – | Save the frame on screen to `~/Pictures` as a PNG. Replies once the file is written: `{"ok":true,"path":"…"}`. `503` if there's no frame yet. |

### Review (the rewind buffer)

| Endpoint | Body | Does |
| --- | --- | --- |
| `POST /v1/play` | switch | `{"on":true}` plays, `{"on":false}` pauses. Pausing from live enters review on the newest frame (`503` if nothing is buffered). Live already counts as playing, so `{"on":true}` there does nothing. With no body: same as Space. |
| `POST /v1/live` | – | Back to live; closes an open file, returning to where you were. |
| `POST /v1/step` | `{"frames": n}` | Step by `n` frames (negative = back). Stepping back from live enters review. Forward from live does nothing. `n` can't be 0. |
| `POST /v1/step-size` | `{"frames": n}` | The step size the app's own ←/→ use: one of `options.step_frames`. Doesn't affect `/v1/step`, which takes its own count. |
| `POST /v1/jump` | `{"seconds": s}` | Jump relative to the playhead (negative = back), clamped to the buffer. Backward from live enters review. |
| `POST /v1/seek` | `{"seconds": s}` | Go to `s` seconds from the start of the timeline, clamped to it. Enters review if live. |

### Clips

Marking and saving need **clip mode**; without it these return `409`.

| Endpoint | Body | Does |
| --- | --- | --- |
| `POST /v1/clip` | switch | Open or close clip mode. Opening from live enters review on the newest frame. Closing drops the marks (and stays in review). |
| `POST /v1/clip/start` | – | Mark the frame on screen as the clip's first frame. |
| `POST /v1/clip/end` | – | Mark the frame on screen as the clip's last frame. |
| `POST /v1/clip/clear` | – | Remove both marks. |
| `POST /v1/clip/save` | – | Write the marked range (or the whole buffer/file if nothing is marked) to `~/Movies`. Replies when written: `{"ok":true,"path":"…","frames":1518}`. Can take several seconds for long clips; status polls keep working meanwhile. |
| `POST /v1/clip/copy` | – | Same export, to a temp file that is put on the clipboard (macOS: as a file; elsewhere its path). Replies `{"ok":true,"frames":n}`. |
| `POST /v1/clip/copy-frame` | – | Copy the frame on screen as a PNG image. |

### Importing

| Endpoint | Body | Does |
| --- | --- | --- |
| `POST /v1/import` | `{"path": "/abs/file.mp4"}` or `{"url": "https://…"}` | Open a video in place of the live feed. Replies at once; follow progress in `status.file` (`mode` is `import` until `file.ready`, then `review`). URLs are downloaded with yt-dlp into `~/Movies`. `404` if the file doesn't exist; `400` for a URL that isn't http(s). Use `/v1/live` to close the file or cancel the import. |

### Setup

Changing any of these restarts capture or the encoder, which **clears the rewind buffer**, so they return `409` while reviewing or with a file open (unless the value is already selected). Go live first.

| Endpoint | Body | Does |
| --- | --- | --- |
| `POST /v1/video-device` | `{"id": "…"}` | Capture from a device from `options.video_devices`. |
| `POST /v1/format` | `{"label": "…"}` | Pick a mode from `options.formats` (the `label`, exactly). |
| `POST /v1/audio-device` | `{"name": "…"}` or `{"name": null}` | Monitor and record from an audio input, or turn audio off. The field must be present: an empty body is a `400`, not "off". |
| `POST /v1/codec` | `{"id": "hevc"}` | One of `options.codecs`: `ffv1`, `prores`, `hevc`, `h264`. |
| `POST /v1/buffer` | `{"minutes": 5}` | Rewind buffer length: one of `options.buffer_minutes`. |

### View

These never touch the buffer and work in any state.

| Endpoint | Body | Does |
| --- | --- | --- |
| `POST /v1/mute` | switch | Mute the audio monitor. |
| `POST /v1/fullscreen` | switch | Fullscreen the window. |
| `POST /v1/stats` | switch | The stats overlay. |
| `POST /v1/hide-overlays` | switch | Hide everything drawn over the picture (the `H` key). |
| `POST /v1/aspect` | `{"id": "16:9"}` | One of `options.aspects`: `captured`, `4:3`, `16:9`. |

## Security

The server is only reachable from the same machine, but any web page you visit can make your browser send requests to `localhost`, so the API defends itself:

- Requests with an **`Origin`** header (every cross-site browser request) are refused with `403`.
- Requests whose **`Host`** header isn't `localhost`, `127.0.0.1` or `::1` are refused (this blocks DNS-rebinding attacks).
- There is no authentication: **any program running as any user on the machine can control Frameback.** Don't expose the port with a tunnel or proxy.

Consequence for plugins: call the API from your plugin's **Node process**, not from browser code such as a property inspector page (browsers add `Origin`, so the call would be refused). Fetch `/v1/options` in the plugin and pass the lists to the inspector through the SDK's messaging.

## Writing a Stream Deck plugin

Frameback has no push channel, so the plugin polls `/v1/status` and redraws keys from it. Every `POST` also returns the new status, so key presses update the display immediately without waiting for the next poll.

```ts
const BASE = `http://127.0.0.1:${process.env.FRAMEBACK_API_PORT ?? 47800}/v1`;

type Status = { mode: "live" | "review" | "clip" | "import"; recording: { seconds: number } | null /* … */ };

async function call(path: string, body?: object): Promise<any> {
  const res = await fetch(`${BASE}/${path}`, {
    method: body === undefined ? "GET" : "POST",
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(130_000), // clip/save can take a while
  });
  const json = await res.json();
  if (!res.ok) throw Object.assign(new Error(json.error), { status: res.status });
  return json;
}

// Poll for state; a refused connection just means Frameback isn't running.
async function poll(): Promise<Status | null> {
  try { return await call("status"); } catch { return null; }
}
```

Suggested behaviour:

| Situation | What to do |
| --- | --- |
| Connection refused / timeout | Frameback isn't running (or the API is off/on another port). Grey the key out and keep polling every few seconds. |
| `409`, `503`, `400` on a press | Show the key's "alert" state; `error` is a sentence you can show as a tooltip or title. |
| `2xx` | Show the key's "ok" state if you want feedback, and redraw from `status`. |
| Poll interval | 500 ms–1 s while keys are visible; the status document is small. |
| Version | `status.version` is the app version. The API is versioned by path (`/v1`); additions (new fields, new endpoints) won't break `/v1` clients, so ignore fields you don't know. |

Key ideas and the calls behind them:

| Key | Press | Show |
| --- | --- | --- |
| Record | `POST record` (flip) | Red while `recording`, with `recording.seconds` as the title |
| Pause / Play | `POST play` (flip) | `playing`; live shows as playing |
| Go live | `POST live` | Highlight when `mode !== "live"` |
| Step ±N | `POST step {frames: ±N}` | Disabled when `timeline` is `null` |
| Skip ±5 s | `POST jump {seconds: ±5}` | – |
| Dial | `POST seek {seconds}` from `timeline.length` × dial position | `timeline.position` |
| Clip | `POST clip` (flip) | Highlight when `mode === "clip"` |
| Mark in / out | `POST clip/start`, `POST clip/end` | Show `clip.start` / `clip.end` |
| Save clip | `POST clip/save` | Alert on error; `path` on success |
| Mute | `POST mute` (flip) | `audio.muted` |
| Screenshot | `POST screenshot` | – |
| Codec / buffer / device pickers | the setup endpoints | Choices from `/v1/options`; selected is in `status.settings` / `status.capture` |

Multi-action presses that depend on state (for example "save the clip, then go live") should check the reply to each call rather than assuming success: the app refuses when its state doesn't allow an action, and that is reported, not queued.
