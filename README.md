# ch-phi-rust

用 Rust + [wgpu](https://wgpu.rs/) 复刻 Phigros 谱面渲染（判定线、音符与 `blockAreaList` 块系统），
用 [sasa](https://github.com/Mivik/sasa) 播放音乐与打击音效，并支持把整曲导出为视频。

> 非官方项目，仅供学习/研究。谱面、音乐、图片等素材版权归原作者所有。

## 特性

- **判定线 / 音符**：tap / drag / flick / hold，事件缓动（15 张缓动表）、`floorPosition`、
  按 `bpm` 与速度事件推进。
- **块系统（blockAreaList）**：Normal / Subtract / Disabled / Ready 四种状态，
  移植 `BlockCompose` / `EdgeMask` / `GlowMask` / `ActiveBlock` / `DisabledBlock` / `ReadyBlock` 等
  着色器（位移扰动、边缘/辉光、减块"挖洞"、预乘 alpha 合成）。
- **HUD**：暂停图标、进度线、连击、分数（CHCAT 计分）、打击特效（`hit.png` 6×5 图集）与金色火花。
- **打击音效**：默认 `tap`/`drag`/`flick`；`--random-sfx` 开启后每次命中从 `assets/audio/idk/`
  随机取一个（播放与导出一致）。
- **视频导出**：`--recorder` 离屏渲染 60 FPS 并管道给 ffmpeg（优先硬件编码），
  打击音效自动混入音乐。
- **画面缩放**：`--size` 控制游戏画面比例，播放与导出均生效。

## 依赖

- **Rust**：edition 2024（建议 stable ≥ 1.85）。
- **ffmpeg**：仅 `--recorder` 导出需要。放运行目录（`ffmpeg.exe`），或用 `--ffmpeg` 指定。
- 运行平台：Windows / Linux / macOS（CI 构建见 [发布](#发布)）。
  Windows 发布版为纯 GUI 程序（无控制台窗口）。

`assets/` 下除 `charts/` 外的资源都会在编译期由 `build.rs` 内嵌进二进制；
`assets/charts/` 为运行时从磁盘读取的谱面目录，不参与内嵌。

## 构建

```bash
cargo build --release
# 产物：target/release/ch-phi-rust（Windows 为 ch-phi-rust.exe）
```

## 使用

```text
ch-phi-rust [谱面路径] [选项]
```

| 选项 | 说明 |
| --- | --- |
| `--recorder [WxH]` | 视频导出（需 ffmpeg）；可选导出尺寸，默认 `1440x1080` |
| `--random-sfx`, `--idk` | 随机打击音效：每次命中从 `assets/audio/idk/` 随机取一个 |
| `--size <比例>` | 游戏画面缩放比例（> 0，`1.0` 表示铺满），播放与导出均生效 |
| `--chart <路径>` | 谱面 JSON 文件（等价于位置参数） |
| `--music <路径>` | 音乐文件（默认自动查找谱面目录下的 `Music.*`） |
| `--output`, `--out <路径>` | 导出视频输出路径（仅 `--recorder`） |
| `--ffmpeg <路径>` | ffmpeg 可执行文件路径 |
| `--frames <数量>` | 最多导出的帧数（仅 `--recorder`） |
| `-h`, `--help` | 显示帮助 |

路径参数均支持绝对/相对路径（相对当前工作目录）。

### 示例

```bash
# 播放默认谱面（assets/charts/DesultorySignals.technoplanet.0/Chart.EZ.json）
ch-phi-rust

# 播放指定谱面
ch-phi-rust assets/charts/DesultorySignals.technoplanet.0/Chart.AT.json

# 随机打击音效
ch-phi-rust --random-sfx assets/charts/DesultorySignals.technoplanet.0/Chart.IN.json

# 导出 1440x1080 视频（随机打击音效）
ch-phi-rust --recorder 1440x1080 --random-sfx --output out.mp4

# 只导出前 300 帧做预览
ch-phi-rust --recorder 1280x720 --frames 300
```

- 播放：关闭窗口退出。
- 导出：弹出进度窗口，按 `Esc` 可取消导出。

## 谱面目录

`--chart` 指向谱面 JSON；同目录下需有音乐与（可选）背景图：

```text
assets/charts/<曲名>/
├─ Chart.EZ.json / Chart.HD.json / Chart.IN.json / Chart.AT.json   # 难度
├─ Music.0.wav（或任意 Music.*）                                     # 音乐（ffmpeg 可解码的格式）
├─ IllustrationBlur.0.png                                            # 背景，可选
└─ Info.json                                                         # 曲目信息，可选
```

谱面为 RPE 风格：顶层 `formatVersion` / `offset`，含 `judgeLineList`（判定线与音符、
速度/位移/旋转等事件）与 `blockAreaList`（方块区域及缩放/旋转/移动事件）。

## 目录结构（源码）

```text
src/
├─ main.rs          # 参数解析、初始化音频/谱面并启动
├─ app.rs           # 窗口与事件循环，独立线程持续渲染
├─ renderer.rs      # wgpu 资源、绘制 API、块着色器与合成管线
├─ render_block.rs  # blockAreaList 块的阶段判定/缓动/变换
├─ render_chart.rs  # 判定线与音符、HUD、连击/分数、打击音效
├─ export.rs        # --recorder 视频导出（离屏渲染 + ffmpeg + 音效混音）
├─ chart_fv.rs      # 谱面类型定义
├─ line.rs / fps.rs # 线段数据与帧率统计
└─ embedded.rs      # 编译期内嵌资源（由 build.rs 生成）
build.rs            # 把 assets/（除 charts/）内嵌进二进制
```

## 发布

`.github/workflows/release.yml`：push `v*` 标签（或在 Actions 手动触发并填 `tag`）后，
在 Windows / Linux / macOS 上构建 release，并创建一个 **草稿（draft）** Release，上传各平台压缩包。

```bash
git tag v0.1.0
git push origin v0.1.0
```

## 素材与版权

- 音乐、插画与谱面**不在仓库内**（`assets/charts/` 已在 `.gitignore` 中排除），请自行放入。
- 其余内嵌素材（音符、打击特效、打击音效、UI、图标等）源自 **Phigros**，
  版权归 **Phigros 官方（Pigeon Games / 鸽游 / 南京鸽游网络有限公司）** 及相应原作者所有；
  此处仅用于学习/研究，请勿用于任何商业用途。
- 本项目为民间非官方作品，与 Pigeon Games 无关联。

## 致谢

- 红区（噪域）逆向[chcat-docs](https://docs.chcat1320.top/knowladge/phigros/)。
- 音频/窗口/GPU 分别基于 [sasa](https://github.com/Mivik/sasa)、
  [winit](https://github.com/rust-windowing/winit)、[wgpu](https://wgpu.rs/)。
