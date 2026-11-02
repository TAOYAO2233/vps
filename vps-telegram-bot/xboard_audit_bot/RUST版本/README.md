# Xboard 日志审计 Rust 版

本目录是 Xboard 日志审计项目的 Rust 实现。VPS 上只部署这套 Rust 项目：主机运行 Rust 程序，客户端 VPS 只通过 rsyslog TLS 转发日志。主机与客户端共用 `manage.sh`；只有主机使用 `/etc/xboard-audit/.env`。

## 当前交付边界

- **没有在本地编译、运行或测试。**代码需在你的 GitHub Actions 中编译和验证。
- 仓库原有的 `.github/workflows/build.yml` 保持不变，只构建 `media-control-bot`。将本地 `xboard_audit_bot/xboard-audit.yml` 新增到仓库根目录的 `.github/workflows/xboard-audit.yml`，它只构建本项目。
- `manage.sh` 顶部四个二进制下载 URL/SHA-256 变量故意留空。从 Actions 下载构建产物并上传到自己的网盘后，填写可由 VPS 直接下载的固定 HTTPS 文件地址和对应 SHA-256。地址为空时安装/升级会明确报错，脚本不会现场编译。

## GitHub Actions 应执行的步骤

`xboard-audit.yml` 在仓库相对路径 `vps-telegram-bot/xboard_audit_bot/RUST版本` 下运行 `cargo test --locked`，构建 Linux `x86_64-unknown-linux-musl` 静态二进制并生成 SHA-256。若 `Cargo.lock` 缺失，首次 CI 会生成并作为 Actions artifact 上传；请下载后提交到仓库，以便之后的构建固定依赖版本。分支提交、PR 或手动运行只生成 Actions artifact，不创建 GitHub Release。下载 artifact 后，将其中的 `xboard-audit-linux-x86_64-musl` 上传到网盘，并从同一 artifact 的 `.sha256` 文件取校验值。工作流目前没有构建 `aarch64`，因此 ARM 主机的下载地址应保持为空，直到另行添加对应构建。

二进制应命名为 `xboard-audit`，主机 SH 脚本将其安装至 `/usr/local/bin/xboard-audit`。客户端无需二进制。

## 主机安装

1. 在 `manage.sh` 顶部填写网盘上二进制文件的固定 HTTPS 直链与 SHA-256；不要填会变化的分享页地址或浮动文件地址。脚本用 `curl` 下载文件并校验哈希，链接若返回 HTML、要求网页登录或文件内容变化，安装会失败。
2. 将脚本放到 Ubuntu 20.04+ 主机并以 root 运行 `bash manage.sh install-master`，或运行 `bash manage.sh` 使用菜单。
3. 输入 Bot Token 和逗号分隔的 Chat ID。脚本创建 `.env`、证书、rsyslog 配置、节点日志目录、systemd 服务和每 24 小时清理一次的定时器。
4. 在防火墙中只允许已授权 VPS 访问主机 TCP/6514。主机无需开放 UDP/514。
5. 安装时脚本先用 `logger` 验证 rsyslog 规则，再要求在 60 秒内产生一次真实 xboard-node 连接，核对 journald 中的日志已写入主机节点文件。验证失败会明确报错，已写入的组件会保留供排障；修正后可运行 `bash manage.sh verify-source` 重试，也可运行 `uninstall` 清理部分安装。
6. 使用 `bash manage.sh test-telegram` 发送测试消息，使用 `bash manage.sh status` 查看状态。

如主机 rsyslog 没有接收 `xboard-node` 的 journald 消息，先检查本机发行版的 rsyslog/journald 输入配置。脚本不会覆盖发行版的全局 rsyslog 配置。

## 添加客户端 VPS

菜单流程：在主机运行 `bash manage.sh`，选 `6) 添加 VPS 节点`，依次登记 `HK`、`US-LOS`；菜单会在每次操作后重新显示。先将 `manage.sh` 放到各副机并运行，然后选 `2) 安装客户端`。脚本会创建 `/etc/xboard-audit/import` 并暂停等待；此时用另一个 SSH/SFTP 会话把主机生成的完整 `export-HK` 或 `export-US-LOS` 文件夹上传到对应副机的该目录。回到菜单按 Enter，脚本扫描证书包，再询问主机公网 IP，完成安装后清理上传副本。选择 `0` 退出菜单。菜单不能代替跨主机复制证书包。

1. 主机运行 `bash manage.sh add-node <节点ID>`，例如 `vps-b`。节点 ID 以字母开头，只能含字母、数字、`_`、`-`。
2. 主机会生成 `/etc/xboard-audit/export-<节点ID>`。**此目录含客户端私钥**，只通过受保护的 SSH/SCP 连接复制到对应 VPS。
3. 客户端菜单选 `2` 后创建接收目录，上传证书包、按 Enter 扫描，再输入主机公网 IP。若使用非交互命令，也可运行 `bash manage.sh install-client <证书包目录> <主机公网IP>`。客户端不创建 `.env`，不安装 Rust 程序。
4. 客户端脚本会发送一条带节点标记的测试日志。检查主机 `/var/log/xboard-audit/nodes/<节点ID>.log` 收到该条日志和后续真实 xboard-node 日志后，从主机删除导出的证书包副本。

所有 VPS 使用 SSH 密钥登录时，可从持有登录私钥的管理电脑执行以下示例（替换 IP、用户名和密钥路径）：

```bash
# 主机登记节点后，先从主机取回该节点证书包和脚本
scp -i ~/.ssh/vps_key -r root@主机IP:/etc/xboard-audit/export-vps-b ./export-vps-b
scp -i ~/.ssh/vps_key root@主机IP:/usr/local/sbin/xboard-audit-manage ./manage.sh

# 先将脚本放到副机，并在副机运行菜单选 2；脚本创建接收目录后再上传证书包
scp -i ~/.ssh/vps_key ./manage.sh root@副机IP:/root/
scp -i ~/.ssh/vps_key -r ./export-vps-b root@副机IP:/etc/xboard-audit/import/
# 回到副机菜单按 Enter 扫描，然后输入主机公网 IP
```

`-i` 后应填写**SSH 登录私钥**，不是证书包内的 `client.key`。如果不同 VPS 使用不同 SSH 密钥，分别在对应的 `scp`、`ssh` 命令中指定即可。当前 `add-node` 只在主机登记并生成证书包，不会自动 SSH 登录副机。

客户端配置持久 rsyslog 发送队列。主机校验客户端证书名称在已登记节点列表中；移除节点后，该证书名称不再允许接入。已授权但被攻陷的客户端仍可能伪造另一个节点的日志标记，因此应保护每台 VPS 的 root 权限和客户端私钥。

## 日常管理

| 命令 | 功能 |
| --- | --- |
| `bash manage.sh interval 60` | 修改主机消息汇总间隔，10–3600 秒。 |
| `bash manage.sh retention 48` | 修改主机日志保留小时数，1–8760 小时。 |
| `bash manage.sh telegram` | 修改 Token 或 Chat ID，Token 输入不回显。 |
| `bash manage.sh add-node vps-c` | 添加客户端节点并生成 TLS 证书包。 |
| `bash manage.sh remove-node vps-c` | 停止接收节点，保留历史日志。 |
| `bash manage.sh upgrade` | 下载并校验已配置的发布二进制。 |
| `bash manage.sh maintenance` | 手动执行日志轮转与过期清理。 |
| `bash manage.sh uninstall` | 卸载本机由此脚本安装的全部组件。 |
| `bash manage.sh verify-source` | 主机等待并核对真实 xboard-node 连接日志。 |
| `bash manage.sh renew-server` | 续期主机 TLS 证书。 |
| `bash manage.sh renew-node vps-c` | 为已登记节点生成客户端证书续期包。 |

只有主机有 `.env`，默认 `PUSH_INTERVAL_SECONDS=60`、`LOG_RETENTION_HOURS=48`。配置修改后脚本重启 Rust 服务。维护定时器每天 UTC+8 00:00 触发一次；主机关机错过任务时，开机后会补做一次。维护按日志内层时间戳删除超过 48 小时的行，同一文件中较新的行会保留。无法解析时间戳的行以文件最后修改时间判断。清理期间 Telegram 发送会等待；若已有请求正在发送，维护任务先等该请求结束，清理完成后自动恢复发送。维护不会改写当前正在写入的 `.log`，也会跳过未读完、最近一小时内修改或仍被 rsyslog 打开的轮转文件，留待下次处理。因此磁盘上可能保留超过 48 小时的日志，尤其是在定时任务刚运行完后的 24 小时内。清理时通过临时文件原子替换，保留原文件权限和属主。

## 卸载

先在**每台客户端**运行 `bash manage.sh uninstall`，最后在主机运行同一命令；也可使用菜单的“卸载本机全部组件”。脚本会显示将删除的内容，并要求输入 `UNINSTALL`。主机卸载会停止并移除审计服务和清理定时器，删除本工具的 rsyslog 规则、二进制、`.env`、证书、节点日志、轮转日志与持久化状态；客户端卸载会移除转发规则、证书和专用发送队列。脚本会先验证移除规则后的 rsyslog 配置，再重启 rsyslog；验证或重启失败时恢复原规则。脚本只删除自身创建的专用目录，并要求存在安装标记。

卸载不会移除系统共用的 rsyslog、OpenSSL 等软件包，也不会改动其它 VPS。系统 journal 中已写入的历史服务日志、Telegram 已发送消息和你自行保存的脚本/证书包副本无法由本机卸载脚本单独清除；如需清除副本，应在其保存位置另行处理。

## 解析和消息语义

程序按节点及事务 ID 配对 `inbound connection from` 和带 UUID 的 `inbound connection to`；`outbound/direct` 不计入。重复目标逐条保留。Telegram 摘要按节点、UUID、来源 IP 和协议分组，时间统一为固定 UTC+8。消息采用纯文本，避免目标地址触发 Markdown 解析；超长摘要按完整访问行拆分。

读取位置、未完成事务、待汇总访问和待发送消息保存在 `/var/lib/xboard-audit/state.json`。重启后按文件轮转顺序补读；发送中断后的历史摘要会标记“延迟补发”，只保留配置时长内的数据。Telegram 返回限流时按 `retry_after` 重试；单个 Chat ID 的临时失败不会阻塞其它接收者。永久拒绝的消息保存在状态文件的失败队列中，直到超过日志保留期；修正 Chat ID 后不会自动改投到新 ID。`bash manage.sh status` 可查看待汇总、待发送和失败数量。发送成功与保存确认之间若发生进程崩溃，可能重复发送一条消息；这是无法由 Telegram `sendMessage` 提供事务确认而消除的窗口。

主机和客户端证书签发后有效期为 825 天。`status` 会提示 30 天内到期的证书；主机可执行 `renew-server`，已登记客户端可通过 `renew-node` 生成新包，再在客户端运行 `install-client` 更新。CA 证书到期前 180 天会提示；CA 轮换需重新分发信任证书及客户端证书，目前不做自动轮换。请先在测试 VPS 验证续期流程。读取队列达到上限时 Rust 会暂停继续读取并在服务日志提示，需尽快排查 Telegram 失败、磁盘和流量峰值。

## 待在 CI 和 VPS 验证

- `cargo test`，包括用户提供的 8 条访问样例、重复目标和 outbound 忽略。
- `cargo build --release` 对目标 Linux 架构成功。
- `rsyslogd -N1` 在主机和客户端通过；TLS 连接、证书名称限制及客户端重连正常。
- 日志轮转期间和服务重启后无漏读；停机期间的旧轮转文件能续读；按内层时间戳清理混合新旧行时，尚未读取的轮转文件不被改写。
- Telegram 大批量分页、限流重试和 `.env` 权限符合预期。
- Chat ID 返回永久错误时其它接收者继续发送；多次轮转后按顺序补读，不跳过中间文件。
- 客户端安装失败时证书和 rsyslog 配置回滚；节点变更后 rsyslog 重启失败时登记回滚；卸载时服务未停止则拒绝删除状态。
- Ubuntu 20.04+ 现场检查 rsyslog 的 journald 输入；不同发行版配置可能需要调整本机采集规则。

部署前请先在测试 VPS 演练，并核对生成的 `/etc/rsyslog.d/`、systemd 和证书文件。此目录尚未包含编译产物，下载地址为空时不能完成主机安装。
