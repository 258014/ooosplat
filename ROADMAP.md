# OOOSplat Roadmap

[中文](ROADMAP.md) | [English](ROADMAP_EN.md)

本路线图用于说明 OOOSplat 的产品方向和实施优先级。P0–P3 表示相对优先顺序，不代表版本号，也不承诺具体发布日期。

当前版本：**0.4.0**。本版本重点完善多类型素材输入、可恢复生成流程与非破坏式 Gaussian 编辑。

## 产品原则

- **一键工作流**：持续减少引擎配置和手动操作，让用户从输入素材直接获得可用的 Gaussian Splatting 结果。
- **本地优先**：重建、训练、预览和导出优先使用用户本机算力，不依赖云端处理服务。
- **安全与非破坏**：素材和工程数据默认保留在本地；编辑和导出尽量保留原始结果，并让文件位置和处理状态清晰可追踪。

## 优先级说明

- **P0 · 近期重点**：优先推进核心体验与重点平台工作。
- **P1 · 高优先级**：扩展输入方式并提升重建性能。
- **P2 · 平台扩展**：将完整桌面工作流带到更多操作系统。
- **P3 · 中长期探索**：面向更多拍摄方式和使用场景进行能力探索。

## 路线图

| 优先级 | 事项 | 目标 | GitHub Issue |
| --- | --- | --- | --- |
| P0 | 大型 Gaussian 预览稳定性与性能 | 持续降低超大 PLY 加载和编辑时的内存峰值，并改善选择、渲染和退出预览的稳定性。 | 待创建 |
| P0 | 速度优化 | 缩短画面准备、特征提取、匹配、重建和训练的总体耗时，同时保持现有结果兼容性。 | 待创建 |
| P0 | 生成质量优化 | 改善相机注册、几何完整性、细节表现和边缘质量，并为低质量结果提供可验证的优化策略。 | 待创建 |
| P1 | 高斯泼溅生成 Benchmark | 建立可复现的素材集、硬件环境、质量指标和耗时指标，用于持续比较版本间的生成速度、资源占用与结果质量。 | 待创建 |
| P1 | 视频拍摄方法提示 UI | 在选择素材和开始生成前提供环绕路径、运动速度、重叠率、光照与避坑提示，帮助用户获得更适合重建的视频。 | 待创建 |
| P1 | 应用内中英文切换 | 支持在设置中切换中文与英文，并覆盖主要界面、操作提示、状态信息和错误摘要；语言选择在重启后保持。 | 待创建 |
| P1 | 重建质量诊断与修复建议 | 将低注册率、图像质量和 GPU 后端错误转换为更明确、可操作的用户提示。 | 待创建 |
| P2 | 导出 Mesh | 将重建结果转换并导出为常见 Mesh 格式，并明确纹理、坐标系与质量选项。 | 待创建 |
| P2 | macOS 签名与公证 | 为 Apple Silicon 安装包接入正式签名和 notarization，改善首次安装体验。 | 待创建 |
| P3 | 全景视频支持 | 探索将全景视频作为输入并生成可用 Gaussian Splatting 结果的工作流。 | 待创建 |

## 已完成

| 状态 | 事项 | 交付 | GitHub Issue |
| --- | --- | --- | --- |
| 已完成 | Ubuntu 24.04 Alpha | 为 x86_64 提供 `.deb` 桌面安装包和 CLI，使用系统 FFmpeg/FFprobe/CPU COLMAP 与安装包内固定版本 Brush；不代表支持其他 Linux 发行版。 | [#5 Add Linux support](https://github.com/ooolabdev/ooosplat/issues/5)，由 [PR #10](https://github.com/ooolabdev/ooosplat/pull/10) 交付 |
| 已完成 | Apple Silicon macOS Alpha | 为 macOS 15+ arm64 提供随应用交付的 FFmpeg、FFprobe、CPU COLMAP 和 Brush 工作流。 | [#4 Add macOS support](https://github.com/ooolabdev/ooosplat/issues/4)，由 [PR #8](https://github.com/ooolabdev/ooosplat/pull/8) 交付 |
| 已完成 | 内嵌高斯泼溅预览 | 已支持加载 `.ply`、相机浏览、整体 Transform、撤销 / 重做、动画预览，以及非破坏式 Gaussian 和竖屏视频导出。 | [#3 关于集成查看功能](https://github.com/ooolabdev/ooosplat/issues/3) |
| 已完成 | COLMAP CUDA 自动加速 | 已支持检测 NVIDIA 驱动和 Compute Capability，满足要求时自动启用 GPU 特征提取与匹配，否则无中断地回退 CPU。 | [#6 Add CUDA-accelerated pipeline for NVIDIA GPUs](https://github.com/ooolabdev/ooosplat/issues/6)；相关用户反馈 [#2](https://github.com/ooolabdev/ooosplat/issues/2) |
| 已完成 · 0.4.0 | 阶段级断点续跑与时间预估 | 中断任务可校验并复用画面、Mask、特征、匹配和稀疏重建检查点；损坏或缺失的阶段自动回退重跑，并提供生成时间估算。 | [PR #27](https://github.com/ooolabdev/ooosplat/pull/27) |
| 已完成 · 0.4.0 | 透明素材自动 Mask | 自动识别透明 MOV 和 PNG，保留 RGBA 数据供 Brush 使用，并生成对应 COLMAP Mask 排除透明背景。 | [PR #19](https://github.com/ooolabdev/ooosplat/pull/19) 及后续实现 |
| 已完成 · 0.4.0 | 支持输入图片序列 | 统一视频与图片输入入口；图片序列使用共享相机、穷举匹配和增量 Mapper，透明 PNG 自动生成 Mask。 | [PR #19](https://github.com/ooolabdev/ooosplat/pull/19) |
| 已完成 · 0.4.0 | Gaussian 区域编辑 | 增加矩形、球形和盒形选择、非破坏式删除、裁切冻结、统一撤销 / 重做，以及保存为 `edit.ply`。 | 0.4.0 实现（待建 Issue） |

## 跟踪与贡献

实际功能范围、技术讨论和实施进度以关联的 GitHub Issue 为准。欢迎在对应 Issue 中补充使用场景、参与讨论或贡献代码。

标记为“待创建”的事项尚无独立 Issue；创建后应将本页对应条目替换为固定的 Issue 编号和链接。路线图会根据项目反馈和实现条件调整，优先级变化不代表功能被取消。
