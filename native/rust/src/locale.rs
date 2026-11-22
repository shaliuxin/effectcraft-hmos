//! 系统语言与 HOME：把鸿蒙的 locale 接进 EffectCraft 上游。
//!
//! EffectCraft 的「跟随系统」界面语言经 `sys-locale`（读 `LANGUAGE`/`LC_ALL`/`LC_MESSAGES`/`LANG`）。
//! 鸿蒙应用进程默认不带这些变量，于是「跟随系统」永远落到英文。ArkTS 侧能拿到真实
//! locale（`i18n.System.getSystemLocale()`），这里把 BCP-47 标签写进 `LANG`，上游「跟随系统」即生效。
//!
//! HOME 统一指向沙箱 `files` 目录：EffectCraft 的 config/自动恢复/字体回退都按 `HOME` 与
//! `$HOME/.fonts` 定位，把它固定到 `files` 保证「写入目录」与「查找目录」一致，并让上游在
//! 沙箱内找到可写空间（OHOS 沙箱外无写权限）。
use std::path::Path;

/// 按界面语言挑 CJK 面。鸿蒙没有日/韩专用面，非繁体一律用 SC（含全部常用汉字）。
fn cjk_face(language: &str) -> &'static str {
    match language {
        "zh-TW" => "HarmonyOS_Sans_TC.ttf",
        _ => "HarmonyOS_Sans_SC.ttf",
    }
}

/// 系统 locale 的 BCP-47 形式（`zh_CN` → `zh-CN`）。`locale` 缺失时退回 `language`。
fn system_tag(locale: &str, language: &str) -> Option<String> {
    let tag = [locale, language].into_iter().map(str::trim).find(|tag| !tag.is_empty())?;
    Some(tag.chars().map(|c| if c == '_' { '-' } else { c }).collect())
}

/// 在 `wc_init` 里、任何 UI 与字体读取之前调用一次。
pub fn install(files_dir: &str, locale: &str, language: &str) {
    let tag = system_tag(locale, language);
    if let Some(tag) = &tag {
        // 上游 sys-locale 按 LANGUAGE → LC_ALL → LC_MESSAGES → LANG 取值：先清掉可能残留的
        // 高优先级变量，再写 LANG（与桌面 Linux 用法一致）。
        for key in ["LANGUAGE", "LC_ALL", "LC_MESSAGES"] {
            std::env::remove_var(key);
        }
        std::env::set_var("LANG", tag);
    }
    // HOME 指向沙箱 files：config/恢复/字体回退与写入同目录。
    let prior_home = std::env::var_os("HOME").map(|s| s.to_string_lossy().into_owned());
    std::env::set_var("HOME", files_dir);
    log::info!("locale: prior HOME={prior_home:?} -> now {files_dir}");

    // best-effort：把鸿蒙系统 CJK 面挂到 $HOME/.fonts 与 $HOME/.local/share/fonts，供上游
    // linux 分支的字体扫描兜底。仅尽力而为，失败不阻塞首帧（首版界面为英文即可用）。
    let tag_str = tag.as_deref().unwrap_or_default();
    for base in [
        Path::new(files_dir).join(".fonts"),
        Path::new(files_dir).join(".local/share/fonts"),
    ] {
        let _ = stage_fonts(&base, tag_str);
    }
}

/// 把要用的系统字体挂到 `base` 下（`noto-cjk/NotoSansCJK-Regular.ttc`）。
fn stage_fonts(base: &Path, language: &str) -> Result<(), String> {
    std::fs::create_dir_all(base).map_err(|e| format!("create .fonts: {e}"))?;
    let rel = "noto-cjk/NotoSansCJK-Regular.ttc";
    let source = Path::new("/system/fonts").join(cjk_face(language));
    if !source.is_file() {
        return Err(format!("no {} in /system/fonts", cjk_face(language)));
    }
    let link = base.join(rel);
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    if std::fs::symlink_metadata(&link).is_ok() {
        return Ok(());
    }
    // 沙箱常禁软链：退回复制。
    if std::os::unix::fs::symlink(&source, &link).is_err() {
        log::info!("locale: staging {} as {}", source.display(), link.display());
        std::fs::copy(&source, &link).map_err(|e| format!("copy {}: {e}", link.display()))?;
    }
    Ok(())
}
