#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{bail, Context, Result};
use eframe::egui;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

// ───────────────────────── 共通ヘルパー ─────────────────────────

fn command(program: &str) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

fn powershell(script: &str) -> Result<String> {
    let full = format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8; {script}");
    let out = command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &full])
        .output()
        .context("PowerShell を起動できません (Windows 専用機能です)")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn fmt_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", v, U[i])
}

fn is_admin() -> bool {
    command("net").args(["session"]).output().map(|o| o.status.success()).unwrap_or(false)
}

fn is_admin_cached() -> bool {
    use std::sync::OnceLock;
    static A: OnceLock<bool> = OnceLock::new();
    *A.get_or_init(is_admin)
}

fn letter(s: &str) -> Option<char> {
    s.trim().chars().next().filter(|c| c.is_ascii_alphabetic()).map(|c| c.to_ascii_uppercase())
}

/// 並列ディレクトリ走査(jwalk)。(パス, サイズ, 更新日時) を返す
fn walk_files(root: &str) -> Vec<(PathBuf, u64, SystemTime)> {
    jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((e.path(), m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
        })
        .collect()
}

// ───────────────────────── 共有状態 ─────────────────────────

#[derive(Clone, Default)]
struct Drive {
    letter: char,
    size: u64,
    free: u64,
    label: String,
    fs: String,
}

#[derive(Clone, Default)]
struct Row {
    a: String,
    b: String,
}

#[derive(Default)]
struct Shared {
    log: Vec<String>,
    rows: Vec<Row>,
    drives: Vec<Drive>,
    clean_list: Vec<PathBuf>,
    dup_groups: Vec<Vec<PathBuf>>,
    empty_list: Vec<PathBuf>,
    busy: bool,
}

type Sh = Arc<Mutex<Shared>>;

fn log(sh: &Sh, s: impl Into<String>) {
    sh.lock().unwrap().log.push(s.into());
}

fn set_rows(sh: &Sh, rows: Vec<Row>) {
    sh.lock().unwrap().rows = rows;
}

fn size_row(s: u64, p: &Path) -> Row {
    Row { a: fmt_size(s), b: p.display().to_string() }
}

fn list_drives() -> Result<Vec<Drive>> {
    let out = powershell(
        "Get-CimInstance Win32_LogicalDisk | Where-Object {$_.DriveType -eq 3} | \
         ForEach-Object { \"$($_.DeviceID)|$($_.Size)|$($_.FreeSpace)|$($_.VolumeName)|$($_.FileSystem)\" }",
    )?;
    let mut v = vec![];
    for line in out.lines() {
        let p: Vec<&str> = line.trim().split('|').collect();
        if p.len() < 5 {
            continue;
        }
        v.push(Drive {
            letter: p[0].chars().next().unwrap_or('?'),
            size: p[1].parse().unwrap_or(0),
            free: p[2].parse().unwrap_or(0),
            label: p[3].to_string(),
            fs: p[4].to_string(),
        });
    }
    Ok(v)
}

// ───────────────────────── タスク: ドライブ/合体 ─────────────────────────

fn task_drives(sh: &Sh) -> Result<()> {
    let d = list_drives()?;
    log(sh, format!("{} 個のドライブを検出しました", d.len()));
    sh.lock().unwrap().drives = d;
    Ok(())
}

fn part_info(letter: char) -> Result<(u32, u64, u64)> {
    let out = powershell(&format!(
        "Get-Partition -DriveLetter {letter} | ForEach-Object {{ \"$($_.DiskNumber)|$($_.Offset)|$($_.Size)\" }}"
    ))?;
    let p: Vec<&str> = out.trim().split('|').collect();
    if p.len() < 3 {
        bail!("{letter}: のパーティション情報を取得できません");
    }
    Ok((p[0].parse()?, p[1].parse()?, p[2].parse()?))
}

fn task_merge(sh: &Sh, from: char, into: char, execute: bool) -> Result<()> {
    if from == into {
        bail!("同じドライブは指定できません");
    }
    let drives = list_drives()?;
    let d = drives.iter().find(|x| x.letter == from).context("統合元ドライブが見つかりません")?;
    let c = drives.iter().find(|x| x.letter == into).context("統合先ドライブが見つかりません")?;
    let used = d.size.saturating_sub(d.free);
    log(sh, format!("{}: 使用量 {} / {}: 空き {}", from, fmt_size(used), into, fmt_size(c.free)));

    if c.free < used + used / 10 + (1 << 30) {
        bail!("{}: の空きが不足しています(統合元の使用量+10%+1GB が必要)", into);
    }
    log(sh, "✔ 容量チェックOK");

    let (cd, coff, csize) = part_info(into)?;
    let (dd, doff, _) = part_info(from)?;
    if cd != dd {
        bail!("別々の物理ディスク上にあるため、パーティション合体はできません。『ジャンクション』タブをご利用ください");
    }
    let cend = coff + csize;
    if doff < cend {
        bail!("{}: が {}: より前にあります。拡張できるのは右隣の領域のみです", from, into);
    }
    let others = powershell(&format!(
        "Get-Partition -DiskNumber {cd} | ForEach-Object {{ \"$($_.Offset)|$($_.Size)|$($_.Type)\" }}"
    ))?;
    for l in others.lines() {
        let p: Vec<&str> = l.trim().split('|').collect();
        if p.len() < 3 {
            continue;
        }
        let off: u64 = p[0].parse().unwrap_or(0);
        if off >= cend && off < doff {
            bail!("C と D の間に別パーティション({})があります。隣接していないため標準の拡張はできません", p[2]);
        }
    }
    log(sh, "✔ 同一ディスク・隣接パーティションを確認");

    let backup = format!("{}:\\D_Backup", into);
    let script = format!("select volume {from}\ndelete volume override\nselect volume {into}\nextend\nexit\n");
    log(sh, "─ 実行計画 ─");
    log(sh, format!("1) {}:\\ の全データを {} へ並列コピー(robocopy /MT:32)", from, backup));
    log(sh, format!("2) {}: を削除し、空き領域を {}: に拡張(diskpart)", from, into));
    log(sh, format!("diskpart スクリプト:\n{script}"));

    if !execute {
        log(sh, "※ これは計画チェックのみです(何も変更していません)");
        return Ok(());
    }
    if !is_admin() {
        bail!("管理者権限が必要です。アプリを『管理者として実行』してください");
    }

    log(sh, "コピー中…(データ量により時間がかかります)");
    let st = command("robocopy")
        .args([
            &format!("{from}:\\"),
            &backup,
            "/E",
            "/COPY:DAT",
            "/R:1",
            "/W:1",
            "/MT:32",
            "/NFL",
            "/NDL",
            "/NP",
            "/XD",
            &format!("{from}:\\$RECYCLE.BIN"),
            &format!("{from}:\\System Volume Information"),
        ])
        .status()?;
    let code = st.code().unwrap_or(16);
    if code >= 8 {
        bail!("robocopy が失敗しました(終了コード {code})。D は削除していません");
    }
    log(sh, format!("✔ バックアップ完了(終了コード {code})"));

    let tmp = std::env::temp_dir().join("drivemerge_diskpart.txt");
    fs::write(&tmp, script)?;
    let out = command("diskpart").arg("/s").arg(&tmp).output()?;
    log(sh, String::from_utf8_lossy(&out.stdout).to_string());
    let _ = fs::remove_file(&tmp);
    if !out.status.success() {
        bail!("diskpart が失敗しました。バックアップは {backup} に残っています");
    }
    log(sh, format!("🎉 完了。D のデータは {backup} にあります"));
    Ok(())
}

fn task_restore(sh: &Sh, src: String, dst: String) -> Result<()> {
    if !Path::new(&src).exists() {
        bail!("コピー元 {src} が存在しません");
    }
    log(sh, format!("{src} → {dst} へ並列コピー中…"));
    let st = command("robocopy")
        .args([&src, &dst, "/E", "/COPY:DAT", "/R:1", "/W:1", "/MT:32", "/NFL", "/NDL", "/NP"])
        .status()?;
    let code = st.code().unwrap_or(16);
    if code >= 8 {
        bail!("robocopy が失敗しました(終了コード {code})");
    }
    log(sh, format!("✔ 復元完了(終了コード {code})"));
    Ok(())
}

fn task_link(sh: &Sh, target: String, link: String) -> Result<()> {
    if Path::new(&link).exists() {
        bail!("リンク先 {link} は既に存在します");
    }
    let out = command("cmd").args(["/C", "mklink", "/J", &link, &target]).output()?;
    log(sh, String::from_utf8_lossy(&out.stdout).trim().to_string());
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

// ───────────────────────── タスク: 解析 ─────────────────────────

fn task_usage(sh: &Sh, root: String) -> Result<()> {
    let t = Instant::now();
    let files = walk_files(&root);
    let rootp = Path::new(&root);
    let mut map: HashMap<String, u64> = HashMap::new();
    for (p, s, _) in &files {
        let rel = p.strip_prefix(rootp).unwrap_or(p);
        let first = rel
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        *map.entry(first).or_default() += s;
    }
    let mut v: Vec<(String, u64)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    set_rows(sh, v.into_iter().map(|(n, s)| Row { a: fmt_size(s), b: n }).collect());
    log(sh, format!("{} ファイルを {:.2} 秒で集計", files.len(), t.elapsed().as_secs_f32()));
    Ok(())
}

fn task_ext(sh: &Sh, root: String) -> Result<()> {
    let files = walk_files(&root);
    let mut map: HashMap<String, (u64, u64)> = HashMap::new();
    for (p, s, _) in &files {
        let ext = p
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
            .unwrap_or_else(|| "(拡張子なし)".into());
        let e = map.entry(ext).or_default();
        e.0 += 1;
        e.1 += s;
    }
    let mut v: Vec<_> = map.into_iter().collect();
    v.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    set_rows(
        sh,
        v.into_iter().take(300).map(|(e, (n, s))| Row { a: fmt_size(s), b: format!("{e}   ({n} 件)") }).collect(),
    );
    log(sh, format!("{} ファイルを拡張子別に集計", files.len()));
    Ok(())
}

fn task_big(sh: &Sh, root: String, min_mb: u64) -> Result<()> {
    let mut v: Vec<_> = walk_files(&root).into_iter().filter(|(_, s, _)| *s >= min_mb << 20).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(300);
    log(sh, format!("{} 件の大きいファイルを検出", v.len()));
    set_rows(sh, v.iter().map(|(p, s, _)| size_row(*s, p)).collect());
    Ok(())
}

fn task_old(sh: &Sh, root: String, days: u64, min_mb: u64) -> Result<()> {
    let limit = SystemTime::now() - Duration::from_secs(days * 86400);
    let now = SystemTime::now();
    let mut v: Vec<_> =
        walk_files(&root).into_iter().filter(|(_, s, t)| *t < limit && *s >= min_mb << 20).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(500);
    log(sh, format!("{} 日以上更新されていないファイル: {} 件(上位500件表示)", days, v.len()));
    set_rows(
        sh,
        v.iter()
            .map(|(p, s, t)| {
                let d = now.duration_since(*t).map(|d| d.as_secs() / 86400).unwrap_or(0);
                Row { a: fmt_size(*s), b: format!("{d}日前  {}", p.display()) }
            })
            .collect(),
    );
    Ok(())
}

fn task_search(sh: &Sh, root: String, query: String) -> Result<()> {
    let q = query.to_lowercase();
    if q.is_empty() {
        bail!("検索語を入力してください");
    }
    let v: Vec<_> = walk_files(&root)
        .into_par_iter()
        .filter(|(p, _, _)| {
            p.file_name().map(|n| n.to_string_lossy().to_lowercase().contains(&q)).unwrap_or(false)
        })
        .collect();
    log(sh, format!("「{query}」に一致: {} 件(上位1000件表示)", v.len()));
    set_rows(sh, v.iter().take(1000).map(|(p, s, _)| size_row(*s, p)).collect());
    Ok(())
}

// ───────────────────────── タスク: 重複 ─────────────────────────

fn hash_file(p: &Path, limit: Option<usize>) -> Option<[u8; 32]> {
    let mut f = fs::File::open(p).ok()?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut remaining = limit.unwrap_or(usize::MAX);
    while remaining > 0 {
        let want = buf.len().min(remaining);
        let n = f.read(&mut buf[..want]).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        remaining -= n;
    }
    Some(*h.finalize().as_bytes())
}

fn task_dupes(sh: &Sh, root: String, min_kb: u64) -> Result<()> {
    let t = Instant::now();
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for (p, s, _) in walk_files(&root) {
        if s >= min_kb << 10 {
            by_size.entry(s).or_default().push(p);
        }
    }
    let cands: Vec<(u64, Vec<PathBuf>)> = by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();
    log(sh, format!("サイズ一致の候補 {} グループ → 並列ハッシュ中…", cands.len()));

    // 先頭64KBで絞り込み → 一致したものだけ全体をハッシュ(全コア並列)
    let groups: Vec<(u64, Vec<PathBuf>)> = cands
        .par_iter()
        .flat_map(|(size, paths)| {
            let mut part: HashMap<[u8; 32], Vec<&PathBuf>> = HashMap::new();
            for p in paths {
                if let Some(h) = hash_file(p, Some(65536)) {
                    part.entry(h).or_default().push(p);
                }
            }
            let mut out: Vec<(u64, Vec<PathBuf>)> = vec![];
            for (_, ps) in part.into_iter().filter(|(_, v)| v.len() > 1) {
                let mut full: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
                for p in ps {
                    if let Some(h) = hash_file(p, None) {
                        full.entry(h).or_default().push(p.clone());
                    }
                }
                for (_, mut g) in full {
                    if g.len() > 1 {
                        g.sort_by_key(|p| p.as_os_str().len());
                        out.push((*size, g));
                    }
                }
            }
            out
        })
        .collect();

    let mut rows = vec![];
    let mut wasted = 0u64;
    let mut store = vec![];
    for (size, g) in groups {
        wasted += size * (g.len() as u64 - 1);
        for (i, p) in g.iter().enumerate() {
            rows.push(Row {
                a: if i == 0 { fmt_size(size) } else { "  〃".into() },
                b: format!("{}{}", if i == 0 { "[残す] " } else { "[削除] " }, p.display()),
            });
        }
        rows.push(Row::default());
        store.push(g);
    }
    log(sh, format!("重複グループ {} 件 / 無駄な容量 約 {} ({:.2}秒)", store.len(), fmt_size(wasted), t.elapsed().as_secs_f32()));
    let mut g = sh.lock().unwrap();
    g.rows = rows;
    g.dup_groups = store;
    Ok(())
}

fn task_dupe_delete(sh: &Sh) -> Result<()> {
    let groups = std::mem::take(&mut sh.lock().unwrap().dup_groups);
    let (mut ok, mut freed) = (0u32, 0u64);
    for g in &groups {
        for p in g.iter().skip(1) {
            let sz = p.metadata().map(|m| m.len()).unwrap_or(0);
            if fs::remove_file(p).is_ok() {
                ok += 1;
                freed += sz;
            }
        }
    }
    log(sh, format!("重複ファイル {} 件を削除し、{} 解放しました", ok, fmt_size(freed)));
    set_rows(sh, vec![]);
    Ok(())
}

// ───────────────────────── タスク: 空フォルダ ─────────────────────────

fn task_empty_scan(sh: &Sh, root: String) -> Result<()> {
    let mut dirs = vec![];
    let mut nonempty: HashSet<PathBuf> = HashSet::new();
    for e in jwalk::WalkDir::new(&root).skip_hidden(false).into_iter().filter_map(|e| e.ok()) {
        let p = e.path();
        if let Some(par) = p.parent() {
            nonempty.insert(par.to_path_buf());
        }
        if e.file_type().is_dir() {
            dirs.push(p);
        }
    }
    let rootp = PathBuf::from(&root);
    let empties: Vec<PathBuf> = dirs.into_iter().filter(|d| *d != rootp && !nonempty.contains(d)).collect();
    log(sh, format!("空フォルダ {} 件(入れ子の空フォルダは繰り返しスキャンで消せます)", empties.len()));
    let mut g = sh.lock().unwrap();
    g.rows = empties.iter().take(1000).map(|p| Row { a: "空".into(), b: p.display().to_string() }).collect();
    g.empty_list = empties;
    Ok(())
}

fn task_empty_delete(sh: &Sh) -> Result<()> {
    let list = std::mem::take(&mut sh.lock().unwrap().empty_list);
    let ok = list.iter().filter(|p| fs::remove_dir(p).is_ok()).count();
    log(sh, format!("{} 個の空フォルダを削除しました", ok));
    set_rows(sh, vec![]);
    Ok(())
}

// ───────────────────────── タスク: クリーナー ─────────────────────────

#[derive(Clone, Copy)]
struct CleanOpt {
    temp: bool,
    chrome: bool,
    edge: bool,
}

fn clean_dirs(o: CleanOpt) -> Vec<PathBuf> {
    let mut v = vec![];
    let local = std::env::var("LOCALAPPDATA").ok().map(PathBuf::from);
    if o.temp {
        v.push(std::env::temp_dir());
        v.push(PathBuf::from(r"C:\Windows\Temp"));
        if let Some(l) = &local {
            v.push(l.join("Temp"));
        }
    }
    if let Some(l) = &local {
        if o.chrome {
            v.push(l.join(r"Google\Chrome\User Data\Default\Cache"));
            v.push(l.join(r"Google\Chrome\User Data\Default\Code Cache"));
        }
        if o.edge {
            v.push(l.join(r"Microsoft\Edge\User Data\Default\Cache"));
            v.push(l.join(r"Microsoft\Edge\User Data\Default\Code Cache"));
        }
    }
    v.sort();
    v.dedup();
    v
}

fn task_clean_scan(sh: &Sh, days: u64, o: CleanOpt) -> Result<()> {
    let limit = SystemTime::now() - Duration::from_secs(days * 86400);
    let mut list: Vec<(PathBuf, u64)> = vec![];
    for d in clean_dirs(o) {
        if let Some(s) = d.to_str() {
            list.extend(walk_files(s).into_iter().filter(|(_, _, t)| *t < limit).map(|(p, s, _)| (p, s)));
        }
    }
    let total: u64 = list.iter().map(|x| x.1).sum();
    log(sh, format!("削除候補 {} 件 / 合計 {}", list.len(), fmt_size(total)));
    let mut g = sh.lock().unwrap();
    g.rows = list.iter().take(500).map(|(p, s)| size_row(*s, p)).collect();
    g.clean_list = list.into_iter().map(|x| x.0).collect();
    Ok(())
}

fn task_clean_run(sh: &Sh) -> Result<()> {
    let list = std::mem::take(&mut sh.lock().unwrap().clean_list);
    let results: Vec<u64> = list
        .par_iter()
        .map(|p| {
            let sz = p.metadata().map(|m| m.len()).unwrap_or(0);
            if fs::remove_file(p).is_ok() {
                sz
            } else {
                u64::MAX
            }
        })
        .collect();
    let ok = results.iter().filter(|&&x| x != u64::MAX).count();
    let freed: u64 = results.iter().filter(|&&x| x != u64::MAX).sum();
    log(sh, format!("{} 件削除、{} 解放しました(使用中のファイルはスキップ)", ok, fmt_size(freed)));
    set_rows(sh, vec![]);
    Ok(())
}

fn task_recycle(sh: &Sh) -> Result<()> {
    powershell("Clear-RecycleBin -Force -ErrorAction SilentlyContinue")?;
    log(sh, "ゴミ箱を空にしました");
    Ok(())
}

// ───────────────────────── タスク: ベンチ/健康 ─────────────────────────

fn task_bench(sh: &Sh, drive: char, mb: u64) -> Result<()> {
    let path = format!("{drive}:\\drivemerge_bench.tmp");
    let chunk: Vec<u8> = (0..(1u32 << 20)).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();

    log(sh, format!("{drive}: に {mb}MB のテストファイルを書き込み中…"));
    let t = Instant::now();
    {
        let mut f = fs::File::create(&path).with_context(|| format!("{path} を作成できません"))?;
        for _ in 0..mb {
            f.write_all(&chunk)?;
        }
        f.sync_all()?;
    }
    let w = mb as f64 / t.elapsed().as_secs_f64();

    let t = Instant::now();
    {
        let mut f = fs::File::open(&path)?;
        let mut buf = vec![0u8; 1 << 20];
        while f.read(&mut buf)? > 0 {}
    }
    let r = mb as f64 / t.elapsed().as_secs_f64();
    let _ = fs::remove_file(&path);

    set_rows(
        sh,
        vec![
            Row { a: "シーケンシャル書込".into(), b: format!("{w:.0} MB/s") },
            Row { a: "シーケンシャル読込".into(), b: format!("{r:.0} MB/s (OSキャッシュの影響で高めに出ることがあります)") },
        ],
    );
    log(sh, "ベンチマーク完了");
    Ok(())
}

fn task_health(sh: &Sh) -> Result<()> {
    let out = powershell(
        "Get-PhysicalDisk | ForEach-Object { \"$($_.FriendlyName)|$($_.MediaType)|$($_.BusType)|$($_.HealthStatus)|$($_.Size)\" }",
    )?;
    let rows = out
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.trim().split('|').collect();
            if p.len() < 5 {
                return None;
            }
            Some(Row {
                a: p[0].to_string(),
                b: format!("{} / {} / 状態: {} / {}", p[1], p[2], p[3], fmt_size(p[4].parse().unwrap_or(0))),
            })
        })
        .collect();
    set_rows(sh, rows);
    log(sh, "ディスクの健康状態を取得しました");
    Ok(())
}

// ───────────────────────── GUI ─────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Drives,
    Merge,
    Restore,
    Link,
    Usage,
    Ext,
    Big,
    Old,
    Dupes,
    Search,
    Empty,
    Clean,
    Bench,
    Health,
}

struct App {
    sh: Sh,
    tab: Tab,
    from: String,
    into: String,
    confirm: String,
    agree: bool,
    link_target: String,
    link_path: String,
    restore_src: String,
    restore_dst: String,
    path: String,
    query: String,
    min_mb: u64,
    min_kb: u64,
    days: u64,
    dup_agree: bool,
    clean_opt: CleanOpt,
    bench_drive: String,
    bench_mb: u64,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        for p in [r"C:\Windows\Fonts\meiryo.ttc", r"C:\Windows\Fonts\YuGothR.ttc", r"C:\Windows\Fonts\msgothic.ttc"] {
            if let Ok(bytes) = fs::read(p) {
                fonts.font_data.insert("jp".into(), egui::FontData::from_owned(bytes));
                for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts.families.get_mut(&fam).unwrap().insert(0, "jp".into());
                }
                break;
            }
        }
        cc.egui_ctx.set_fonts(fonts);
        Self {
            sh: Arc::new(Mutex::new(Shared::default())),
            tab: Tab::Drives,
            from: "D".into(),
            into: "C".into(),
            confirm: String::new(),
            agree: false,
            link_target: r"D:\".into(),
            link_path: r"C:\D".into(),
            restore_src: r"C:\D_Backup".into(),
            restore_dst: r"C:\Restored".into(),
            path: r"C:\Users".into(),
            query: String::new(),
            min_mb: 500,
            min_kb: 100,
            days: 365,
            dup_agree: false,
            clean_opt: CleanOpt { temp: true, chrome: false, edge: false },
            bench_drive: "C".into(),
            bench_mb: 512,
        }
    }

    fn spawn<F>(&self, ctx: &egui::Context, f: F)
    where
        F: FnOnce(&Sh) -> Result<()> + Send + 'static,
    {
        {
            let mut g = self.sh.lock().unwrap();
            if g.busy {
                return;
            }
            g.busy = true;
        }
        let sh = self.sh.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Err(e) = f(&sh) {
                log(&sh, format!("❌ {e:#}"));
            }
            sh.lock().unwrap().busy = false;
            ctx.request_repaint();
        });
    }

    fn path_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("対象フォルダ:");
            ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(400.0));
            if ui.button("参照…").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.path = p.display().to_string();
                }
            }
        });
    }
}

fn export_csv(rows: &[Row]) {
    if let Some(p) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_file_name("result.csv").save_file() {
        let mut s = String::from("\u{feff}");
        for r in rows {
            s.push_str(&format!("\"{}\",\"{}\"\n", r.a.replace('"', "\"\""), r.b.replace('"', "\"\"")));
        }
        let _ = fs::write(p, s);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let busy = self.sh.lock().unwrap().busy;
        if busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (t, n) in [
                    (Tab::Drives, "💽 ドライブ"),
                    (Tab::Merge, "🔗 C+D合体"),
                    (Tab::Restore, "♻ 復元"),
                    (Tab::Link, "📂 ジャンクション"),
                    (Tab::Usage, "📊 使用量"),
                    (Tab::Ext, "🧩 拡張子別"),
                    (Tab::Big, "🐘 巨大"),
                    (Tab::Old, "🕸 古いファイル"),
                    (Tab::Dupes, "👯 重複"),
                    (Tab::Search, "🔎 検索"),
                    (Tab::Empty, "📭 空フォルダ"),
                    (Tab::Clean, "🧹 クリーナー"),
                    (Tab::Bench, "⚡ ベンチ"),
                    (Tab::Health, "❤ 健康状態"),
                ] {
                    if ui.selectable_label(self.tab == t, n).clicked() {
                        self.tab = t;
                        self.sh.lock().unwrap().rows.clear();
                    }
                }
            });
        });

        egui::TopBottomPanel::bottom("log").resizable(true).min_height(120.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.strong("ログ");
                if busy {
                    ui.spinner();
                    ui.label("処理中…");
                }
                if ui.button("クリア").clicked() {
                    self.sh.lock().unwrap().log.clear();
                }
            });
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for l in self.sh.lock().unwrap().log.iter() {
                    ui.monospace(l);
                }
                ui.set_min_width(ui.available_width());
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            match self.tab {
                Tab::Drives => {
                    if ui.add_enabled(!busy, egui::Button::new("🔄 更新")).clicked() {
                        self.spawn(ctx, task_drives);
                    }
                    ui.separator();
                    for d in self.sh.lock().unwrap().drives.iter() {
                        let used = d.size.saturating_sub(d.free);
                        let frac = if d.size > 0 { used as f32 / d.size as f32 } else { 0.0 };
                        ui.label(format!(
                            "{}:  {}  [{}]  使用 {} / 全体 {}",
                            d.letter, d.label, d.fs, fmt_size(used), fmt_size(d.size)
                        ));
                        ui.add(egui::ProgressBar::new(frac).text(format!("空き {}", fmt_size(d.free))));
                        ui.add_space(6.0);
                    }
                }
                Tab::Merge => {
                    ui.heading("C ドライブと D ドライブを合体");
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 120, 0),
                        "⚠ D を削除して C に拡張します。必ず重要データを別媒体にもバックアップしてください。",
                    );
                    ui.label("条件: 同一ディスク上で D が C の右隣にあること / C の空きが D の使用量より十分あること");
                    ui.horizontal(|ui| {
                        ui.label("統合元(消える側):");
                        ui.add(egui::TextEdit::singleline(&mut self.from).desired_width(30.0));
                        ui.label("統合先(広がる側):");
                        ui.add(egui::TextEdit::singleline(&mut self.into).desired_width(30.0));
                    });
                    let ok = letter(&self.from).zip(letter(&self.into));
                    if ui.add_enabled(!busy && ok.is_some(), egui::Button::new("🔍 計画チェック(変更なし)")).clicked() {
                        let (f, i) = ok.unwrap();
                        self.spawn(ctx, move |sh| task_merge(sh, f, i, false));
                    }
                    ui.separator();
                    ui.checkbox(&mut self.agree, "データ消失のリスクを理解し、バックアップ済みです");
                    ui.horizontal(|ui| {
                        ui.label("確認のため MERGE と入力:");
                        ui.text_edit_singleline(&mut self.confirm);
                    });
                    let ready = ok.is_some() && self.agree && self.confirm == "MERGE";
                    if ui
                        .add_enabled(!busy && ready, egui::Button::new("🚀 合体を実行").fill(egui::Color32::from_rgb(160, 40, 40)))
                        .clicked()
                    {
                        let (f, i) = ok.unwrap();
                        self.confirm.clear();
                        self.agree = false;
                        self.spawn(ctx, move |sh| task_merge(sh, f, i, true));
                    }
                    if !is_admin_cached() {
                        ui.label("※ 実行には管理者権限が必要です(右クリック→管理者として実行)");
                    }
                }
                Tab::Restore => {
                    ui.heading("バックアップの復元(並列コピー)");
                    ui.label("合体時に作られた C:\\D_Backup を、別の場所(新しいドライブ等)へ戻します。");
                    ui.horizontal(|ui| {
                        ui.label("コピー元:");
                        ui.text_edit_singleline(&mut self.restore_src);
                    });
                    ui.horizontal(|ui| {
                        ui.label("コピー先:");
                        ui.text_edit_singleline(&mut self.restore_dst);
                        if ui.button("参照…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                self.restore_dst = p.display().to_string();
                            }
                        }
                    });
                    if ui.add_enabled(!busy, egui::Button::new("復元を実行")).clicked() {
                        let (s, d) = (self.restore_src.clone(), self.restore_dst.clone());
                        self.spawn(ctx, move |sh| task_restore(sh, s, d));
                    }
                }
                Tab::Link => {
                    ui.heading("ジャンクションで D を C 配下に表示(非破壊)");
                    ui.label("パーティションはそのままで、C:\\D を開くと D の中身が見えるようになります。");
                    ui.horizontal(|ui| {
                        ui.label("実体(D側):");
                        ui.text_edit_singleline(&mut self.link_target);
                    });
                    ui.horizontal(|ui| {
                        ui.label("リンク(C側):");
                        ui.text_edit_singleline(&mut self.link_path);
                    });
                    if ui.add_enabled(!busy, egui::Button::new("作成")).clicked() {
                        let (t, l) = (self.link_target.clone(), self.link_path.clone());
                        self.spawn(ctx, move |sh| task_link(sh, t, l));
                    }
                }
                Tab::Usage => {
                    self.path_row(ui);
                    if ui.add_enabled(!busy, egui::Button::new("集計(並列)")).clicked() {
                        let p = self.path.clone();
                        self.spawn(ctx, move |sh| task_usage(sh, p));
                    }
                }
                Tab::Ext => {
                    self.path_row(ui);
                    if ui.add_enabled(!busy, egui::Button::new("拡張子別に集計")).clicked() {
                        let p = self.path.clone();
                        self.spawn(ctx, move |sh| task_ext(sh, p));
                    }
                }
                Tab::Big => {
                    self.path_row(ui);
                    ui.horizontal(|ui| {
                        ui.label("最小サイズ(MB):");
                        ui.add(egui::DragValue::new(&mut self.min_mb).range(1..=100000));
                        if ui.add_enabled(!busy, egui::Button::new("検索")).clicked() {
                            let (p, m) = (self.path.clone(), self.min_mb);
                            self.spawn(ctx, move |sh| task_big(sh, p, m));
                        }
                    });
                }
                Tab::Old => {
                    self.path_row(ui);
                    ui.horizontal(|ui| {
                        ui.label("更新から(日):");
                        ui.add(egui::DragValue::new(&mut self.days).range(1..=36500));
                        ui.label("最小サイズ(MB):");
                        ui.add(egui::DragValue::new(&mut self.min_mb).range(0..=100000));
                        if ui.add_enabled(!busy, egui::Button::new("検索")).clicked() {
                            let (p, d, m) = (self.path.clone(), self.days, self.min_mb);
                            self.spawn(ctx, move |sh| task_old(sh, p, d, m));
                        }
                    });
                }
                Tab::Dupes => {
                    self.path_row(ui);
                    ui.horizontal(|ui| {
                        ui.label("最小サイズ(KB):");
                        ui.add(egui::DragValue::new(&mut self.min_kb).range(1..=1000000));
                        if ui.add_enabled(!busy, egui::Button::new("検出(blake3・全コア並列)")).clicked() {
                            let (p, m) = (self.path.clone(), self.min_kb);
                            self.spawn(ctx, move |sh| task_dupes(sh, p, m));
                        }
                    });
                    let has = !self.sh.lock().unwrap().dup_groups.is_empty();
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.dup_agree, "[削除]と表示されたファイルを完全に削除することに同意");
                        if ui.add_enabled(!busy && has && self.dup_agree, egui::Button::new("🗑 重複を削除")).clicked() {
                            self.dup_agree = false;
                            self.spawn(ctx, task_dupe_delete);
                        }
                    });
                }
                Tab::Search => {
                    self.path_row(ui);
                    ui.horizontal(|ui| {
                        ui.label("ファイル名に含む:");
                        ui.text_edit_singleline(&mut self.query);
                        if ui.add_enabled(!busy, egui::Button::new("検索")).clicked() {
                            let (p, q) = (self.path.clone(), self.query.clone());
                            self.spawn(ctx, move |sh| task_search(sh, p, q));
                        }
                    });
                }
                Tab::Empty => {
                    self.path_row(ui);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!busy, egui::Button::new("スキャン")).clicked() {
                            let p = self.path.clone();
                            self.spawn(ctx, move |sh| task_empty_scan(sh, p));
                        }
                        let has = !self.sh.lock().unwrap().empty_list.is_empty();
                        if ui.add_enabled(!busy && has, egui::Button::new("🗑 空フォルダを削除")).clicked() {
                            self.spawn(ctx, task_empty_delete);
                        }
                    });
                }
                Tab::Clean => {
                    ui.label("古いキャッシュ/一時ファイルを並列で削除します(使用中のものは自動スキップ)。");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.clean_opt.temp, "Temp");
                        ui.checkbox(&mut self.clean_opt.chrome, "Chrome キャッシュ");
                        ui.checkbox(&mut self.clean_opt.edge, "Edge キャッシュ");
                    });
                    ui.horizontal(|ui| {
                        ui.label("何日以上前:");
                        ui.add(egui::DragValue::new(&mut self.days).range(0..=3650));
                        if ui.add_enabled(!busy, egui::Button::new("スキャン")).clicked() {
                            let (d, o) = (self.days, self.clean_opt);
                            self.spawn(ctx, move |sh| task_clean_scan(sh, d, o));
                        }
                        let has = !self.sh.lock().unwrap().clean_list.is_empty();
                        if ui.add_enabled(!busy && has, egui::Button::new("🗑 削除実行")).clicked() {
                            self.spawn(ctx, task_clean_run);
                        }
                        if ui.add_enabled(!busy, egui::Button::new("ゴミ箱を空にする")).clicked() {
                            self.spawn(ctx, task_recycle);
                        }
                    });
                }
                Tab::Bench => {
                    ui.label("指定ドライブに一時ファイルを書き込み/読み込みして速度を測ります(終了後に自動削除)。");
                    ui.horizontal(|ui| {
                        ui.label("ドライブ:");
                        ui.add(egui::TextEdit::singleline(&mut self.bench_drive).desired_width(30.0));
                        ui.label("サイズ(MB):");
                        ui.add(egui::DragValue::new(&mut self.bench_mb).range(64..=16384));
                        let l = letter(&self.bench_drive);
                        if ui.add_enabled(!busy && l.is_some(), egui::Button::new("⚡ 測定")).clicked() {
                            let (l, m) = (l.unwrap(), self.bench_mb);
                            self.spawn(ctx, move |sh| task_bench(sh, l, m));
                        }
                    });
                }
                Tab::Health => {
                    if ui.add_enabled(!busy, egui::Button::new("ディスクの状態を取得")).clicked() {
                        self.spawn(ctx, task_health);
                    }
                }
            }

            if !matches!(self.tab, Tab::Drives | Tab::Merge | Tab::Restore | Tab::Link) {
                ui.separator();
                let rows = self.sh.lock().unwrap().rows.clone();
                if !rows.is_empty() && ui.button("📄 CSV に保存").clicked() {
                    export_csv(&rows);
                }
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new("rows").striped(true).show(ui, |ui| {
                        for r in rows.iter() {
                            ui.label(&r.a);
                            ui.label(&r.b);
                            ui.end_row();
                        }
                    });
                });
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "age PRO",
        opts,
        Box::new(|cc| {
            let app = App::new(cc);
            let sh = app.sh.clone();
            std::thread::spawn(move || {
                let _ = task_drives(&sh);
            });
            Ok(Box::new(app))
        }),
    )
}