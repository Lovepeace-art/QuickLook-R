//! 压缩包列表视图：目录树构建 / 展平 / 排序、行度量、命中、滚动、绘制入口。

use super::*;

/// 目录树里的一个节点（目录或文件）。兄弟顺序在打开时就排好，绘制时只做一次整型 DFS。
pub(crate) struct ArchiveNode {
    /// 显示名（目录不带 `/`，绘制时才补）。
    pub(crate) name: String,
    /// 是否是目录。
    pub(crate) is_dir: bool,
    /// 解压后字节数（目录恒 0）。
    pub(crate) size: u64,
    /// 修改时间（取不到是空串）。
    pub(crate) time: String,
    /// 是否展开（只有目录有意义）。
    pub(crate) expanded: bool,
    /// 子节点下标（打开时就排好序：目录在前，同类按名字小写排序）。
    pub(crate) children: Vec<usize>,
}

/// 内置压缩包视图：整棵目录树 + 滚动行号 + 选中节点。
///
/// 为什么打开时就建树：滚动/折叠/悬停每次都要重新展平，边展平边拼字符串会把大包拖成
/// 每帧毫秒级；预建树之后展平只是一次整型 DFS（几千条目也只是一趟数组遍历）。
pub(crate) struct ArchiveView {
    pub(crate) path: std::path::PathBuf,
    pub(crate) nodes: Vec<ArchiveNode>,
    /// 根的直接子节点。
    pub(crate) roots: Vec<usize>,
    /// 第一行对应的行号（滚动偏移按"行"算）。
    pub(crate) scroll: usize,
    /// 选中的**节点下标**（不是行号：折叠/展开后行号会变，节点不会）。
    pub(crate) selected: Option<usize>,
}

pub(crate) static ARCHIVE: OnceLock<Mutex<Option<ArchiveView>>> = OnceLock::new();

pub(crate) fn archive_slot() -> &'static Mutex<Option<ArchiveView>> {
    ARCHIVE.get_or_init(|| Mutex::new(None))
}

pub(crate) fn is_archive(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "zip" | "cbz" | "jar" | "war"
    )
}

/// 打开压缩包并建目录树（默认全展开）。失败返回 false，调用方退回文本/插件通道。
pub(crate) fn open_archive_view(path: &Path) -> bool {
    let Ok(parsed) = qlr_render::archive::open(path) else {
        log_warn!(SCOPE, "压缩包解析失败：{}", path.display());
        return false;
    };
    let (nodes, roots) = build_archive_tree(&parsed.entries);
    log_info!(
        SCOPE,
        "ARCHIVE {} 个文件夹 · {} 个文件 · 解压后 {}（目录树 {} 个节点）",
        parsed.dirs,
        parsed.files,
        human_size(parsed.uncompressed),
        nodes.len()
    );
    if let Ok(mut slot) = archive_slot().lock() {
        *slot = Some(ArchiveView {
            path: path.to_path_buf(),
            nodes,
            roots,
            scroll: 0,
            selected: None,
        });
    }
    true
}

/// 把中央目录的线性条目表搭成树（1.7.3）。
///
/// 两件事必须在这里做掉，绘制路径就不必再管：
/// 1. **补目录**：很多打包器（含 `python zipfile`、`Compress-Archive` 的部分路径）不写
///    目录条目，只靠条目的 `/` 前缀推断，否则树是散的；
/// 2. **排序**：目录在前、同类按名字小写排，跟资源管理器的习惯一致。
pub(crate) fn build_archive_tree(
    entries: &[qlr_render::archive::Entry],
) -> (Vec<ArchiveNode>, Vec<usize>) {
    let mut nodes: Vec<ArchiveNode> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut dirs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // 先建目录：这样 "src/" 一定排在 "src/main.rs" 前面，不依赖条目顺序。
    for entry in entries.iter().filter(|entry| entry.is_dir) {
        let path = entry.path.trim_end_matches('/');
        if !path.is_empty() {
            ensure_archive_dir(path, &entry.time, &mut nodes, &mut roots, &mut dirs);
        }
    }
    for entry in entries.iter().filter(|entry| !entry.is_dir) {
        let path = entry.path.trim_end_matches('/');
        if path.is_empty() {
            continue;
        }
        let id = nodes.len();
        nodes.push(ArchiveNode {
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            is_dir: false,
            size: entry.size,
            time: entry.time.clone(),
            expanded: false,
            children: Vec::new(),
        });
        match path.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => {
                match ensure_archive_dir(parent, "", &mut nodes, &mut roots, &mut dirs) {
                    Some(pid) => nodes[pid].children.push(id),
                    None => roots.push(id),
                }
            }
            _ => roots.push(id),
        }
    }
    sort_archive_children(&mut nodes);
    sort_archive_ids(&mut roots, &nodes);
    (nodes, roots)
}

/// 确保目录 `full` 在树里，返回它的节点下标（父目录链递归补齐）。
///
/// `time` 只在"由目录条目本身建出来"时有值；补齐出来的中间目录没有时间戳。
///
/// 防御：`full` 末尾的 `/` 一律先去掉——条目表里目录路径本应已经去掉（`archive.rs` 的
/// `clean`），但这里再兜一次：`"a/"` 直接喂进来会 `rsplit_once('/')` 成 (`"a"`, `""`)，
/// 于是凭空多出一个名字为空的目录行（1.7.3 单测逮到过）。
pub(crate) fn ensure_archive_dir(
    full: &str,
    time: &str,
    nodes: &mut Vec<ArchiveNode>,
    roots: &mut Vec<usize>,
    dirs: &mut std::collections::HashMap<String, usize>,
) -> Option<usize> {
    let full = full.trim_end_matches('/');
    if full.is_empty() {
        return None;
    }
    if let Some(&id) = dirs.get(full) {
        return Some(id);
    }
    let (parent, name) = match full.rsplit_once('/') {
        Some((parent, name)) if !parent.is_empty() => (Some(parent), name),
        Some((_, name)) => (None, name),
        None => (None, full),
    };
    let id = nodes.len();
    nodes.push(ArchiveNode {
        name: name.to_string(),
        is_dir: true,
        size: 0,
        time: time.to_string(),
        expanded: true,
        children: Vec::new(),
    });
    dirs.insert(full.to_string(), id);
    match parent {
        Some(parent) => {
            if let Some(pid) = ensure_archive_dir(parent, "", nodes, roots, dirs) {
                nodes[pid].children.push(id);
            } else {
                roots.push(id);
            }
        }
        None => roots.push(id),
    }
    Some(id)
}

/// 目录在前，同类按名字小写排（大小写只用来打破平手，保证顺序稳定）。
pub(crate) fn cmp_archive_nodes(a: &ArchiveNode, b: &ArchiveNode) -> std::cmp::Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

pub(crate) fn sort_archive_ids(ids: &mut [usize], nodes: &[ArchiveNode]) {
    ids.sort_by(|a, b| cmp_archive_nodes(&nodes[*a], &nodes[*b]));
}

pub(crate) fn sort_archive_children(nodes: &mut [ArchiveNode]) {
    for i in 0..nodes.len() {
        // 先把子表搬出来再排：否则同时要 `&mut nodes[i]` 和 `&nodes[..]`。
        let mut kids = std::mem::take(&mut nodes[i].children);
        kids.sort_by(|a, b| cmp_archive_nodes(&nodes[*a], &nodes[*b]));
        nodes[i].children = kids;
    }
}

/// 按展开状态把树展平成"行"：`(节点下标, 缩进层级)`。
pub(crate) fn flatten_archive(nodes: &[ArchiveNode], roots: &[usize]) -> Vec<(usize, usize)> {
    fn walk(nodes: &[ArchiveNode], ids: &[usize], depth: usize, out: &mut Vec<(usize, usize)>) {
        for &id in ids {
            out.push((id, depth));
            let node = &nodes[id];
            if node.is_dir && node.expanded {
                walk(nodes, &node.children, depth + 1, out);
            }
        }
    }
    let mut out = Vec::with_capacity(nodes.len());
    walk(nodes, roots, 0, &mut out);
    out
}

pub(crate) fn clear_archive_view() {
    if let Ok(mut slot) = archive_slot().lock() {
        *slot = None;
    }
}

/// 一行：已经展开成绘制/命中测试直接可用的一组值。
pub(crate) struct ArchiveRow {
    /// 树节点下标（点击折叠时用它定位节点：行号会随折叠变，节点不会）。
    pub(crate) node: usize,
    /// 缩进层级。
    pub(crate) depth: usize,
    pub(crate) is_dir: bool,
    pub(crate) expanded: bool,
    /// 显示名（目录补了 `/`）。
    pub(crate) name: String,
    /// 右列：大小（目录留空）。
    pub(crate) size: String,
    /// 右列：时间。
    pub(crate) time: String,
}

/// 当前可见行（按展开状态把树展平）。
pub(crate) fn archive_rows() -> Vec<ArchiveRow> {
    let Ok(slot) = archive_slot().lock() else {
        return Vec::new();
    };
    let Some(view) = slot.as_ref() else {
        return Vec::new();
    };
    flatten_archive(&view.nodes, &view.roots)
        .into_iter()
        .map(|(node, depth)| {
            let entry = &view.nodes[node];
            ArchiveRow {
                node,
                depth,
                is_dir: entry.is_dir,
                expanded: entry.expanded,
                name: if entry.is_dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name.clone()
                },
                size: if entry.is_dir {
                    String::new()
                } else {
                    human_size(entry.size)
                },
                time: entry.time.clone(),
            }
        })
        .collect()
}

/// 点 `y` 落在第几行（已加上当前滚动偏移）。落在内容区之外返回 `None`。
///
/// 口径必须和 `window::paint_list` 完全一致：起点同样加 `window::text_padding_top()`，
/// 行高同样用 `window::list_row_height()`，否则"看到的行"和"点到的行"会差一行。
pub(crate) fn archive_row_index(content: RECT, scroll: usize, y: i32) -> Option<usize> {
    let top = content.top + window::text_padding_top();
    if y < top || y >= content.bottom {
        return None;
    }
    Some(scroll + ((y - top) / window::list_row_height()) as usize)
}

/// 压缩包列表的点击（1.7.3）：选中这一行；目录再翻转展开。
///
/// 返回 `true` = 这次点击被列表吃掉了。不的话会落到 `on_drag_start`，
/// 用户点一下目录就等于把整个窗口拖走。
pub(crate) fn archive_click(hwnd: HWND, lparam: LPARAM) -> bool {
    if !has_archive_view() {
        return false;
    }
    let (x, y) = client_xy(lparam);
    let layout = frame(hwnd);
    if !point_in(x, y, layout.content) {
        return false;
    }
    let Ok(mut slot) = archive_slot().lock() else {
        return false;
    };
    let Some(view) = slot.as_mut() else {
        return false;
    };
    let Some(index) = archive_row_index(layout.content, view.scroll, y) else {
        return false;
    };
    let flat = flatten_archive(&view.nodes, &view.roots);
    let Some(&(node, _)) = flat.get(index) else {
        // 点在最后一行下面的空白：吃掉，别变成拖窗口。
        return true;
    };
    view.selected = Some(node);
    let (name, is_dir) = (view.nodes[node].name.clone(), view.nodes[node].is_dir);
    if is_dir {
        view.nodes[node].expanded = !view.nodes[node].expanded;
    }
    // 折叠之后行数变少，滚动位置可能越界：按"一屏几行"重新夹一次。
    let total = flatten_archive(&view.nodes, &view.roots).len();
    let per_screen = archive_rows_per_screen(layout.content);
    view.scroll = view.scroll.min(total.saturating_sub(per_screen));
    let (scroll, expanded) = (view.scroll, view.nodes[node].expanded);
    drop(slot);
    if is_dir {
        log_debug!(
            SCOPE,
            "ARCHIVE 点击 {name}/ 展开={expanded}（共 {total} 行，第 {index} 行，scroll={scroll}）"
        );
    }
    window::invalidate(hwnd);
    true
}

/// 压缩包列表的滚轮（1.7.3）：按行滚，到头就停。
///
/// 无论有没有真的动，都返回 `true`（"这个滚轮我处理了"）——否则会继续往下走成
/// 图片缩放或 PDF 翻页。
pub(crate) fn archive_scroll_by(rows: isize) -> bool {
    if !has_archive_view() {
        return false;
    }
    let Some(preview) = with_state(|s| s.preview) else {
        return false;
    };
    let hwnd = preview.raw();
    let layout = frame(hwnd);
    let per_screen = archive_rows_per_screen(layout.content);
    let Ok(mut slot) = archive_slot().lock() else {
        return false;
    };
    let Some(view) = slot.as_mut() else {
        return false;
    };
    let total = flatten_archive(&view.nodes, &view.roots).len();
    let max = total.saturating_sub(per_screen);
    let next = (view.scroll as isize + rows).clamp(0, max as isize) as usize;
    let moved = next != view.scroll;
    view.scroll = next;
    drop(slot);
    if moved {
        log_debug!(
            SCOPE,
            "ARCHIVE 滚动 {next}/{max}（一屏 {per_screen} 行，共 {total} 行）"
        );
        window::invalidate(hwnd);
    }
    true
}

/// 压缩包列表的滚动条信息（内容单位 = 行）。放得下返回 `None`。
pub(crate) fn archive_scroll_info(area: RECT) -> Option<window::ScrollInfo> {
    let per_screen = archive_rows_per_screen(area) as f32;
    let slot = archive_slot().lock().ok()?;
    let view = slot.as_ref()?;
    let total = flatten_archive(&view.nodes, &view.roots).len() as f32;
    window::ScrollInfo::new(view.scroll as f32, per_screen, total)
}

/// 当前是否有内置压缩包视图（`repaint` 用它决定走列表通道还是文本通道）。
pub(crate) fn has_archive_view() -> bool {
    archive_slot()
        .lock()
        .map(|slot| slot.is_some())
        .unwrap_or(false)
}

/// 一屏能放下几行。绘制与滚轮/命中测试共用同一份口径，否则"看到的行"和"点到的行"会错位。
pub(crate) fn archive_rows_per_screen(content: RECT) -> usize {
    ((content.bottom - content.top) / window::list_row_height()).max(1) as usize
}

/// 画压缩包列表（1.7.3）。
pub(crate) fn paint_archive(hwnd: HWND) {
    // 一次性把要用的字段拷出来、**立刻放锁**再往下走。
    // 铁律：`archive_rows()` 自己也要锁 `archive_slot`，在持有该锁时调用它 = 自锁死
    // （表现：窗口一片黑、界面卡住）。1.7.3 踩过，别再把读取塞回锁里。
    let (title, selected, scroll) = {
        let Ok(slot) = archive_slot().lock() else {
            return;
        };
        let Some(view) = slot.as_ref() else {
            return;
        };
        let title = view
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (title, view.selected, view.scroll)
    };
    // 顶栏的悬停/锁状态和文本通道共用一份（1.7.3）：列表视图的顶栏也是同一套按钮。
    let hover = with_state(|s| s.hover_button).flatten();
    let pinned = with_state(|s| s.pinned).unwrap_or(false);
    let hint = hover.map(|button| match button {
        window::ChromeButton::External => "在其他应用打开",
        window::ChromeButton::Close => "关闭预览",
        window::ChromeButton::Pin => {
            if pinned {
                "已阻止关闭（Esc/空格关不掉）"
            } else {
                "阻止关闭（开了就不能取消）"
            }
        }
    });
    // 拖边/缩放后投影要跟上（和文本通道同样的道理，见 repaint 里的注释）。
    sync_shadow();
    let rows = archive_rows();
    let layout = frame(hwnd);
    let rows_per_screen = archive_rows_per_screen(layout.content);
    let path_name = title;
    let start = scroll.min(rows.len());
    let end = (start + rows_per_screen).min(rows.len());
    let page: Vec<window::ListRow<'_>> = rows[start..end]
        .iter()
        .map(|row| window::ListRow {
            depth: row.depth,
            is_dir: row.is_dir,
            expanded: row.expanded,
            name: &row.name,
            size: &row.size,
            time: &row.time,
            selected: selected == Some(row.node),
        })
        .collect();
    let toast = current_toast();
    let chrome = window::Chrome {
        title: &path_name,
        page: None,
        hover,
        hint,
        toast: toast.as_deref(),
        pinned,
    };
    window::paint_list(hwnd, Some(&chrome), Some(layout.content), &page);
    // 滚动条（1.7.3）：`paint_overlays` 里的那条是按 `scroll_info()`（文本行/PDF 页）算的，
    // 压缩包的"内容单位"是自己展平后的行，所以这里单独画一条。
    if layout.bar.is_some()
        && let Some(info) = archive_scroll_info(layout.below_chrome)
    {
        window::paint_scrollbar(hwnd, layout.below_chrome, info, false);
    }
    paint_overlays(hwnd, &layout);
}
