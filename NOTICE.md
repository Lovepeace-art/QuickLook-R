QLRust — 一个用 Rust 重写的 Windows「按空格预览」工具
Copyright (C) 2026 QLRust contributors

本程序是自由软件：你可以依据自由软件基金会发布的 GNU 通用公共许可证
（第 3 版，或你选择的任何更新版本）的条款重新分发和/或修改它。
本程序分发时希望它有用，但不提供任何担保，甚至不包括适销性或特定用途
适用性的默示担保。详见 GNU 通用公共许可证。

你应该已经随本程序收到一份 GNU 通用公共许可证副本（见 LICENSE）；
如果没有，见 <https://www.gnu.org/licenses/>。

---

## 与上游 QuickLook 的关系

QLRust 是**独立重写**：代码不是从 QuickLook 拷贝的，但以下方面大量参考了
[QuickLook](https://github.com/QL-Win/QuickLook)（作者 Paddy Xu，GPL-3.0）：

- 交互契约：空格开关、Esc 关闭、跟随资源管理器选中项、顶栏按钮的含义；
- 便携目录布局（`portable.lock` + `UserData\`）与插件（`.qlplugin`）语义；
- 系统预览处理器的接入方式（`IPreviewHandler`，Office 三件走这条路）。

按 GPL 的口径，本项目以「衍生作品」对待，整体采用 **GPL-3.0-or-later**。

## 第三方组件

| 组件 | 许可 | 用法 |
|---|---|---|
| PDFium | BSD-3-Clause | **可选**：运行时动态加载 `pdfium.dll`，本仓库不分发该 DLL |
| Rust 标准库与 `windows` crate | MIT / Apache-2.0 | 编译期依赖 |

`windows` crate 为 MIT 或 Apache-2.0 双许可；本项目按其条款使用。

## 素材

`docs/screenshots/` 下的截图由本项目自行截取，内容是本项目自己的窗口与本机
测试文件；`assets/` 下的图标由本项目绘制。
