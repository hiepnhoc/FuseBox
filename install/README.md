# Cài Fusebox chạy local trên từng máy

Hai script trong thư mục này tải bản release mới nhất của fork này từ GitHub
(`hiepnhoc/FuseBox`, bản HiBox build từ branch `kiro`), kiểm tra SHA256, cài vào
PATH và đăng ký cho Fusebox tự chạy nền khi đăng nhập máy, lắng nghe ở
`http://127.0.0.1:8317`. **Chạy lại script là update.** Không cần cài Rust,
Docker hay Node.

| | macOS / Linux | Windows |
| --- | --- | --- |
| Script | `install.sh` | `install.ps1` |
| Binary | `~/.local/bin/fusebox` | `%LOCALAPPDATA%\Fusebox\fusebox.exe` |
| Config, token đăng nhập, usage DB | `~/.fusebox/` | `%USERPROFILE%\.fusebox\` |
| Chạy nền | launchd agent `com.fusebox.proxy` (Linux: `systemd --user`) | Task Scheduler, task `Fusebox` |
| Log | `~/Library/Logs/fusebox.log` | `%USERPROFILE%\.fusebox\fusebox.log` |

## Cài hoặc update

**macOS / Linux**

```sh
bash install/install.sh --codex
```

**Windows** (PowerShell, không cần quyền admin)

```powershell
powershell -ExecutionPolicy Bypass -File install\install.ps1 -Codex
```

Chạy thẳng từ GitHub mà không cần clone. URL phải là branch `kiro`: script trên
`main` giống upstream và mặc định tải release của `DigitalPals/Fusebox`, không có HiBox.

```sh
curl -fsSL https://raw.githubusercontent.com/hiepnhoc/FuseBox/kiro/install/install.sh | bash -s -- --codex
```

```powershell
irm https://raw.githubusercontent.com/hiepnhoc/FuseBox/kiro/install/install.ps1 | iex
```

Cờ `--codex` / `-Codex` ghi provider `fusebox` vào `~/.codex/config.toml`
(file cũ được giữ thành `config.toml.bak`). Bỏ cờ này nếu muốn tự sửa tay;
script vẫn in ra đoạn cần dán.

Các cờ khác: `--no-service` / `-NoService` chỉ cài binary, `--force` /
`-Force` cài lại dù đã đúng version, `--uninstall` / `-Uninstall` gỡ service
và binary nhưng giữ `~/.fusebox`. Biến môi trường `FUSEBOX_REPO` đổi repo
lấy release (ví dụ sang fork của mình khi có sửa code riêng),
`FUSEBOX_VERSION` ghim một tag cụ thể. Đặt `FUSEBOX_REPO=DigitalPals/Fusebox`
để lấy bản gốc upstream thay cho bản HiBox.

## Ra bản mới

Release được GitHub Actions build khi push một tag trùng với `version` trong
`Cargo.toml` của branch `kiro`, ví dụ `0.14.0-hibox.1` → tag `v0.14.0-hibox.1`:

```sh
git checkout kiro
# sửa version trong Cargo.toml và dòng version của package fusebox trong Cargo.lock
git commit -am "Prepare v0.14.0-hibox.2"
git tag v0.14.0-hibox.2 && git push origin kiro v0.14.0-hibox.2
```

Workflow build Mac ARM, Mac Intel, Windows x64, Linux x64 và Linux ARM, rồi tạo
Release kèm SHA256SUMS. Xong thì chạy lại script cài trên từng máy.

## Sau khi cài, trên mỗi máy

1. Mở `http://127.0.0.1:8317`, bấm **Connect account**, đăng nhập ChatGPT
   hoặc Claude. Hoặc dùng terminal:

   ```sh
   fusebox --config ~/.fusebox/config.yaml login codex
   ```

   Token nằm trong `~/.fusebox` của máy đó. **Không copy thư mục này giữa
   hai máy**: refresh token của ChatGPT và Claude xoay vòng mỗi lần refresh,
   hai máy dùng chung một token thì máy này refresh xong máy kia bị văng.
   Đăng nhập riêng từng máy, mỗi provider mất chừng một phút.

2. Codex đọc `~/.codex/config.toml`, nội dung giống hệt trên Mac và Win:

   ```toml
   model = "gpt-6-astra"
   model_provider = "fusebox"

   [model_providers.fusebox]
   name = "Fusebox"
   base_url = "http://127.0.0.1:8317/v1"
   wire_api = "responses"
   ```

   Không cần `env_key` vì Fusebox chỉ nghe trên `127.0.0.1` và chưa có
   client key. Nếu sau này thêm client trong dashboard thì thêm
   `env_key = "FUSEBOX_KEY"` và đặt biến môi trường đó.

3. Claude Code, nếu muốn:

   ```sh
   export ANTHROPIC_BASE_URL=http://127.0.0.1:8317
   export ANTHROPIC_AUTH_TOKEN=fusebox
   ```

## Quản lý service

**macOS**

```sh
launchctl kickstart -k gui/$(id -u)/com.fusebox.proxy   # restart
launchctl bootout gui/$(id -u)/com.fusebox.proxy        # stop
tail -f ~/Library/Logs/fusebox.log
```

**Windows**

```powershell
Start-ScheduledTask Fusebox
Stop-ScheduledTask Fusebox; Stop-Process -Name fusebox -Force
Get-Content $env:USERPROFILE\.fusebox\fusebox.log -Tail 50
```

Trên Windows, task chạy `run-fusebox.ps1` ở chế độ ẩn; khi đăng nhập có thể
thấy cửa sổ PowerShell nháy lên một chút rồi tắt, đó là bình thường.
