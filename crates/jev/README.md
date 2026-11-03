# Built-in Jev decision support / Jev tích hợp sẵn

## English

Jev is shipped inside this Zed fork. There is nothing to install or publish.
The binary embeds the dependency-free Node MCP server, registers `jev` at startup,
and writes `server-<content hash>.cjs` to `<Zed data directory>/jev/` on startup, with or without a key. Different builds use separate script files; legacy
unversioned server.cjs is removed when writing a versioned file. Other hashed
versions are retained because cached ACP commands can still reference them. Zed's existing NodeRuntime supplies Node.js (18+).

**Settings → MCP Servers → jev → Configure** (the gear button; the Agent panel's
**Settings** entry opens the same page) opens a JSON form with `api_key` and the
optional `model`, `endpoint` and `timeout` overrides as comments; once saved, it shows
your current settings. The form is Zed's plain JSON editor, so a saved key is visible
there in clear text (mind shared screens). Enter your TypeSafe key and save; Zed
restarts the server automatically with the new environment. No Zed restart is needed.
Only the keys you save are written, so later default changes still apply.
Alternatively edit **user settings**:

```json
{
  "context_servers": {
    "jev": {
      "settings": { "api_key": "YOUR_TYPESAFE_KEY" }
    }
  }
}
```

Alternatively launch Zed with `JEV_API_KEY`. The descriptor explicitly waits for
login-shell environment loading before checking credentials, regardless of node.path
or ignore_system_version. An inherited key is forwarded only to the default TypeSafe
endpoint; a custom endpoint requires an explicit `settings.api_key`. A non-empty
settings key takes precedence. Jev is always registered and starts even without a
key, so the Configure form and agent tools are available immediately. A missing-key
tool call gives the agent setup instructions without a toast or error-level log.
Calls with a key that fail after an HTTP attempt still show failure notifications.
Start a new agent session if Jev is missing right after first launch; saving a key
restarts the server, so a new external session may be needed to refresh its MCP env.
To disable it: `"context_servers": {"jev": {"enabled": false, "settings": {}}}`.
The MCP page's **Uninstall** action only removes the user entry; the shipped default
returns, so disable it instead. Native **Ask** and **Minimal** profiles do not enable
MCP tools: use **Write** or a custom profile with Jev enabled.

Optional settings under `context_servers.jev.settings`:

```json
{
  "api_key": "YOUR_TYPESAFE_KEY",
  "model": "jev-1.14.0",
  "endpoint": "https://api.typesafe.ai/v1/systemone",
  "timeout": 60
}
```

`timeout` is the total HTTP deadline in seconds, including up to three retries for
429/529, exponential backoff, and `Retry-After`. An overridden endpoint receives
your key and context: use a trusted endpoint and trust only reviewed worktrees,
because trusted project settings replace the Jev entry and can choose its endpoint.
Inherited credentials are never forwarded to custom endpoints. Keep keys out of Git.
The key travels separately in `JEV_API_KEY`, which Zed's command debug formatter
redacts; `JEV_SETTINGS` contains only nonsecret options. Existing ACP protocol debug
logs can contain MCP environment secrets, so review those logs before sharing them.

Tools available to the native Zed Agent and MCP-capable ACP agents (Claude Code,
Codex, Gemini CLI and compatible adapters):

- `jev_yes_no`: supply `context`, `question`, and `criteria.true`/`criteria.false`.
  Returns `model` and `probability` (0–1); near 0.5 means uncertain.
- `jev_score`: supply `context`, `question`, and 1–10 ordered `criteria` strings.
  Returns model, fractional score, confidence, legend and probabilities.
- `jev_decide`: supply `state` and a named `questions` map. Returns the raw API
  response including model, answers and usage. Supports Noul, Score, Choice and Box,
  mixed in one request. State/context accept a string, object or array.

```json
{
  "state": {"change": "Adds a database index", "tests": "passed"},
  "questions": {
    "safe": {"type": "noul", "instructions": "Is this safe to deploy?", "criteria": {"true": "Backward compatible", "false": "Breaks existing queries"}},
    "risk": {"type": "score", "instructions": "Rate deployment risk", "criteria": ["Low", "Moderate", "High"]},
    "route": {"type": "choice", "instructions": "Who should review?", "criteria": {"database": "Database changes", "application": "Application changes"}}
  }
}
```

General questions accept structured instructions and criteria descriptions, with
optional Noul criteria/instructions. Choice supports 1–255 options. Image Box
questions use `{"type":"box","instructions":"Locate the mug in photo-id"}` and
`assets: {"photo-id":{"type":"image","format":"png","data_base64":"…"}}`.
Reference the placeholder in state/instructions and choose an image-capable model
in settings. The server supplies `TypeSafe-Danger-Zone-Images: true`. Inline PNG/JPEG
is supported, not multipart uploads or long-context opt-in. Box returns normalized
edges and nullable confidence; ask a companion Noul whether the target exists.

Use narrow questions, concrete criteria and relevant evidence. Low confidence is a
reason to gather information or request review. Context and images go to TypeSafe;
you choose decision thresholds and actions. Local validation enforces finite
numbers, 64 nesting levels, 32 MiB JSON bodies, and question-specific limits. Model
token limits remain enforced remotely. API failures return MCP `isError`; HTTP
400/413/422 expose at most 500 characters of JSON detail/code with keys of at least
eight characters redacted. Other HTTP error bodies are omitted.

### Failure notifications

Once an HTTP request is attempted, failed calls show a Zed workspace notification
for native and ACP agents: network/timeout failures, non-2xx HTTP responses, exhausted
retries and invalid API responses. Local argument-validation failures only return
tool errors to the agent. Identical error kinds are throttled to one notification
per 60 seconds. A 401 notification tells you to check `settings.api_key`.

The server asynchronously appends sanitized events to `<data_dir>/jev/events.jsonl`
via `JEV_EVENTS_PATH`, with a 256 KiB cap. Events contain only timestamp, tool name,
HTTP status/error kind and a fixed short message, never credentials, state, request
bodies or API response details. Zed watches this shared file, so errors from MCP
processes launched by external agents also notify. Event-write failures do not block
tool results and produce at most one generic stderr line per process.

### Discovery with zero user settings

The startup hook calls `jev::init` after `Project::init` and always registers its
`ContextServerDescriptor`, including the settings schema/defaults used by Configure.
The shipped default settings
include `context_servers.jev = {enabled: true, settings: {}}`.
`ProjectSettings::from_settings` keeps that entry;
`ContextServerStore::resolve_all_context_server_settings` reads the merged defaults
for both empty and folder projects, so `configured_server_ids()` includes `jev`.
The native `ContextServerRegistry` consumes the store. ACP's
`mcp_configurations_for_project` also consumes its configured IDs; v1's
`mcp_servers_for_project` and v2's `session_context` forward path, args and explicit
environment. No user entry is needed for discovery. Jev is **local-only**: on remote projects only the native agent gets Jev, running
locally; ACP agents do not receive it there. `remote: true` is unsupported because
the remote headless server has no built-in Jev descriptor. Existing AI-disable,
agent permission and MCP capability rules still apply.
Other Jev preparation failures are logged without preventing Zed startup. Each project
window starts a Node process, even without a key, with additional processes for ACP sessions.
Agent requests with MCP tools enabled (native Write/custom profiles and MCP-capable ACP
sessions) also carry the three Jev tool definitions, about 7.5 KB (roughly 2k tokens), with or without a
key. Disable the entry if you do not use Jev.

### Development

```sh
node --test crates/jev/server.test.cjs
cargo test -p jev
cargo check -p jev -p zed
./script/clippy -p jev -p zed
```

Standalone stdio: `JEV_API_KEY=… node crates/jev/server.cjs`; optional `JEV_SETTINGS`
contains JSON model/endpoint/timeout options, never the key.

API sources: [System One](https://docs.typesafe.ai/api/system-one),
[OpenAPI](https://api.typesafe.ai/openapi.json),
[limits and errors](https://docs.typesafe.ai/api/errors),
[Choice](https://docs.typesafe.ai/primitives/choice),
[Box](https://docs.typesafe.ai/primitives/box).

## Tiếng Việt

Jev đã được tích hợp trong bản Zed này: không cần cài dev extension hay xuất bản gói.
Zed nhúng mã MCP, đăng ký server `jev` khi khởi động và dùng NodeRuntime có sẵn.
Mã được ghi vào `<thư mục dữ liệu Zed>/jev/server-<content hash>.cjs` khi server khởi
động và chỉ cập nhật khi nội dung thay đổi.

Chỉ cần thêm khóa vào **cài đặt người dùng**:

```json
{"context_servers":{"jev":{"settings":{"api_key":"YOUR_TYPESAFE_KEY"}}}}
```

Hoặc mở Zed từ môi trường có `JEV_API_KEY`. Descriptor đợi môi trường login shell
nạp xong trước khi đọc khóa, kể cả khi dùng node.path hoặc ignore_system_version.
Khóa môi trường chỉ được chuyển đến endpoint TypeSafe mặc định; endpoint tùy chỉnh
cần khóa rõ ràng trong `settings.api_key`. Khóa trong settings được ưu tiên.
Jev luôn đăng ký và chạy dù chưa có khóa, nên agent luôn thấy công cụ và mở được
Configure. Thiếu khóa chỉ trả hướng dẫn cấu hình cho agent, không có toast hoặc log
cấp lỗi. Mở **Settings → MCP Servers → jev → Configure** (nút bánh răng; mục
**Settings** trong Agent panel mở cùng trang này). Biểu mẫu JSON có `api_key`, còn
`model`, `endpoint`, `timeout` là chú thích tùy chọn; sau khi lưu, biểu mẫu hiển thị
cấu hình hiện tại, kể cả khóa ở dạng văn bản thường (cẩn thận khi chia sẻ màn hình).
Nhập khóa rồi lưu, server tự khởi động lại với cấu hình mới, không cần khởi động lại
Zed. Chỉ các khóa bạn lưu được ghi, nên thay đổi mặc định sau này vẫn có hiệu lực.
Tạo phiên agent mới nếu Jev chưa xuất hiện ngay sau lần mở đầu hoặc sau khi cấu hình.
Để tắt: `"context_servers":{"jev":{"enabled":false,"settings":{}}}`.
Nút **Uninstall** chỉ xóa cấu hình người dùng nên mặc định sẽ xuất hiện lại; hãy tắt
bằng enabled:false. Profile **Ask** và **Minimal** không bật MCP; dùng **Write** hoặc
profile tùy chỉnh có bật Jev.

Có thể đặt `model` (mặc định `jev-1.14.0`), `endpoint` và `timeout` trong
`context_servers.jev.settings`. `timeout` tính bằng giây, bao gồm thử lại tối đa ba
lần cho HTTP 429/529 với thời gian chờ tăng dần và tuân thủ `Retry-After`.
Endpoint tùy chỉnh nhận khóa và ngữ cảnh; chỉ dùng endpoint và worktree đáng tin
cậy vì cài đặt dự án đã được tin cậy có thể thay endpoint. Không lưu khóa vào Git.
Khóa được tách trong `JEV_API_KEY`, được bộ định dạng log lệnh Zed che giá trị;
log giao thức ACP vẫn có thể chứa khóa, nên kiểm tra trước khi chia sẻ.

- `jev_yes_no`: truyền `context`, `question`, `criteria.true`/`criteria.false`, nhận
  xác suất đúng 0–1. Gần 0,5 nghĩa là chưa chắc chắn.
- `jev_score`: truyền `context`, `question`, 1–10 mức `criteria` từ thấp đến cao;
  nhận điểm thập phân, độ tin cậy, chú giải và phân bố xác suất.
- `jev_decide`: truyền `state` và bản đồ `questions`, nhận phản hồi API đầy đủ.
  Hỗ trợ Noul, Score, Choice (1–255 lựa chọn), Box; `state`/`context` có thể là chuỗi,
  đối tượng hoặc mảng. Box dùng ảnh PNG/JPEG base64 trong `assets` và model hỗ trợ ảnh.

Đặt câu hỏi hẹp với tiêu chí cụ thể; khi độ tin cậy thấp, thu thập thêm bằng chứng
hoặc yêu cầu người xem xét. Ngữ cảnh/ảnh được gửi đến TypeSafe. Giới hạn cục bộ:
32 MiB, 64 tầng lồng nhau và số hữu hạn. Lỗi API trả qua MCP `isError`; HTTP
400/413/422 trả chi tiết/mã lỗi tối đa 500 ký tự với khóa dài ít nhất tám ký tự được
che. Các nội dung lỗi HTTP khác bị bỏ qua.

Jev chỉ chạy cục bộ: trên dự án từ xa, chỉ native Agent nhận Jev chạy ở máy cục bộ;
ACP không nhận server đó. `remote:true` không được hỗ trợ. Khi thiếu khóa, không có thông báo lỗi. Quy tắc tắt AI và quyền agent
vẫn áp dụng. Các bản build dùng tên file theo hash; giữ các bản hash cũ vì lệnh ACP
đã lưu có thể còn tham chiếu chúng, chỉ dọn file server.cjs cũ khi ghi bản mới. Lỗi chuẩn bị Jev được ghi log và không ngăn Zed khởi động. Xem các lệnh kiểm
thử và tài liệu API ở phần English.

Khi yêu cầu HTTP đã được gửi mà thất bại (mạng, timeout, HTTP lỗi, hết lượt thử lại
hoặc phản hồi không hợp lệ), Zed hiển thị thông báo cho cả native và ACP. Lỗi kiểm
tra tham số trước khi gửi chỉ trả về agent, không thông báo. Cùng loại lỗi chỉ báo
một lần trong 60 giây. Server ghi sự kiện đã làm sạch vào `jev/events.jsonl` (giới hạn
256 KiB); Zed theo dõi file chung nên nhận được cả lỗi từ tiến trình do ACP mở.
File không chứa khóa, ngữ cảnh, nội dung yêu cầu hay chi tiết phản hồi API. Lỗi ghi
file không chặn công cụ và chỉ ghi một dòng stderr chung mỗi tiến trình.

Mỗi cửa sổ dự án dùng một tiến trình Node kể cả khi chưa có khóa; phiên ACP có thể
chạy thêm tiến trình MCP riêng. Mỗi yêu cầu agent có bật MCP (profile Write/tùy chỉnh
và phiên ACP hỗ trợ MCP) mang thêm ba định nghĩa công cụ Jev, khoảng 7,5 KB (~2k token), dù có khóa
hay không. Tắt mục jev nếu không dùng.
