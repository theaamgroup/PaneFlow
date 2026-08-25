//! Per-pane TCP listening-port + agent-process detection (EP-005 US-012).
//!
//! One entry point, [`scan_panes`]: given `(terminal_key, root_pid)` pairs,
//! it returns a per-terminal [`PaneScan`] attributing LISTEN ports and
//! recognised agent binaries to each terminal's PTY process subtree. Each
//! port carries an OS-side frontend classification ([`PortEntry`]) derived
//! from the socket-owning process's argv - the sidebar's clickable chips key
//! off this, not off PTY-text scraping (which is timing-dependent and stays
//! enrichment-only: exact URLs, backend labels).
//!
//! Cost contract (US-012): the process table is traversed ONCE per tick -
//! a shared `visited` set spans all roots so no pid is walked twice and each
//! pid's `comm` is read at most once (the pre-refactor code re-walked the
//! descendants once for ports and once for agents, so this is strictly
//! cheaper per tick at any pane count).
//!
//! Implementation: `libc::proc_listchildpids` BFS, `libproc` name +
//! `listpidinfo::<ListFDs>` / `pidfdinfo::<SocketFDInfo>`. Those are
//! naturally per-pid, so per-subtree attribution needs no global socket
//! table - unlike a `/proc/net/tcp`-style scan, which has to build one.
//!
//! BFS (not DFS) ordering is load-bearing: US-013 picks the agent binary
//! NEAREST the subtree root ("the agent you launched, not its children"),
//! which is exactly breadth-first visit order. The walker caps at 512 PIDs
//! per root to bound memory on fork-bombs.

/// One LISTEN port owned by a terminal's subtree.
#[derive(Debug, Clone, PartialEq)]
pub struct PortEntry {
    pub port: u16,
    /// `Some(display_label)` when the socket-owning process's argv matches a
    /// known frontend dev server (Vite, Next.js, …). The OS-side classifier
    /// sees the actual socket owner, so chip clickability no longer depends
    /// on the PTY text scrape having caught the announcement line inside its
    /// scan window.
    pub frontend: Option<&'static str>,
}

/// Per-terminal scan result (EP-005 US-012).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneScan {
    /// LISTEN ports owned by the terminal's subtree, sorted by port number
    /// and deduplicated (a dual-stack v4+v6 bind is one entry).
    pub ports: Vec<PortEntry>,
    /// Recognised agent binary names found in the subtree, in BFS
    /// (root-proximity) order, deduplicated. `first()` is the pane's
    /// identity-pill agent (US-013); the union across panes feeds the
    /// workspace-level `detected_agents` aggregate.
    pub agents: Vec<String>,
    /// Best-effort representative command for surface naming, resolved by the
    /// same off-thread process scan so IPC/UI callers never do process-table
    /// I/O. This is a child-selection heuristic, not a PTY foreground process
    /// group query.
    pub foreground_command: Option<String>,
}

/// Soft cap on PIDs walked per root subtree (fork-bomb bound, both
/// platforms). Checked at dequeue time, so one last fanout batch can
/// overshoot it by up to one process's child count - the bound is
/// "≈512", which is all the memory guarantee needs.
#[cfg(target_os = "macos")]
const MAX_PIDS_PER_ROOT: usize = 512;

// ---------------------------------------------------------------------------
// Platform-neutral pure helpers (unit-tested on every host)
// ---------------------------------------------------------------------------

/// Filter a BFS-ordered stream of process names down to recognised agent
/// binaries, preserving first-seen (nearest-root) order and deduplicating.
/// Exact basename match only - `claude-code-cli` or a wrapper script must
/// not trigger (parity with the historical `AI_PROCESS_NAMES` contract).
///
/// Consumed by the platform `scan_panes` paths and the unit tests.
#[cfg(any(target_os = "macos", test))]
fn agents_in_bfs_order<'a>(
    comms_in_bfs_order: impl Iterator<Item = &'a str>,
    agent_binaries: &[&str],
) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for comm in comms_in_bfs_order {
        if agent_binaries.contains(&comm) && !found.iter().any(|f| f == comm) {
            found.push(comm.to_string());
            if found.len() == agent_binaries.len() {
                break;
            }
        }
    }
    found
}

/// Frontend dev servers recognisable from the socket owner's argv. The table
/// is deliberately frontend-only: a hit arms a CLICKABLE sidebar chip, so
/// precision beats recall here - backend labels keep flowing from the
/// PTY-text enrichment path, where a mislabel is cosmetic.
#[cfg(any(target_os = "macos", test))]
const FRONTEND_ARGV: &[(&str, &str)] = &[
    ("vite", "Vite"),
    ("next", "Next.js"),
    ("nuxt", "Nuxt"),
    ("nuxi", "Nuxt"),
    ("astro", "Astro"),
    ("remix", "Remix"),
    ("webpack-dev-server", "Webpack"),
    ("ng", "Angular"),
    ("react-scripts", "React"),
];

/// Classify a process's argv into a frontend dev-server label.
///
/// Matches per-argument BASENAMES (directory components and `.js`-family
/// extensions stripped) so `node /…/node_modules/.bin/vite` hits while
/// `/srv/invite/server.js` cannot. One special case: Next.js rewrites its
/// process title to `next-server (vX.Y.Z)` - a single argv token, matched by
/// prefix. Only the leading args are inspected; launchers always carry the
/// tool name up front.
#[cfg(any(target_os = "macos", test))]
fn classify_frontend_argv<'a>(args: impl Iterator<Item = &'a str>) -> Option<&'static str> {
    for arg in args.take(8) {
        if arg
            .get(..11)
            .is_some_and(|p| p.eq_ignore_ascii_case("next-server"))
        {
            return Some("Next.js");
        }
        let base = arg.rsplit(['/', '\\']).next().unwrap_or(arg);
        let base = base
            .strip_suffix(".js")
            .or_else(|| base.strip_suffix(".mjs"))
            .or_else(|| base.strip_suffix(".cjs"))
            .or_else(|| base.strip_suffix(".ts"))
            .unwrap_or(base);
        for &(key, label) in FRONTEND_ARGV {
            if base.eq_ignore_ascii_case(key) {
                return Some(label);
            }
        }
    }
    None
}

#[cfg(test)]
fn normalize_process_basename(name: &str) -> &str {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    for suffix in [".exe", ".cmd", ".bat", ".ps1"] {
        if base
            .get(base.len().saturating_sub(suffix.len())..)
            .is_some_and(|s| s.eq_ignore_ascii_case(suffix))
        {
            return &base[..base.len() - suffix.len()];
        }
    }
    base
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

/// macOS ppid→children map over every visible process, built once per scan.
///
/// `libc::proc_listchildpids` is deliberately NOT used: on modern macOS it
/// returns 0 children for an unprivileged caller, so the old per-node subtree
/// walk found nothing and the workspace card never lit its agent dot. Instead
/// we enumerate all pids (`listpids(ProcAllPIDS)`) and read each one's parent
/// from `proc_bsdinfo.pbi_ppid` - the very same `proc_pidinfo(PROC_PIDTBSDINFO)`
/// query that `name()` already succeeds with for same-user processes. Mirrors
/// the parent-map walk. Processes we can't
/// inspect (EPERM on SIP-protected / other-user pids, dead-pid races) are
/// skipped - our agents are same-user PTY children, always readable.
#[cfg(target_os = "macos")]
fn macos_children_map() -> std::collections::HashMap<u32, Vec<u32>> {
    use libproc::libproc::bsd_info::BSDInfo;
    use libproc::libproc::proc_pid::pidinfo;
    use libproc::processes::{ProcFilter, pids_by_type};

    let mut children_of: std::collections::HashMap<u32, Vec<u32>> =
        std::collections::HashMap::new();
    let pids = match pids_by_type(ProcFilter::All) {
        Ok(pids) => pids,
        Err(e) => {
            // Wholesale enumeration failure - NOT a routine per-pid EPERM skip:
            // every port badge and agent dot on macOS goes dark at once. This
            // is the `proc_listchildpids`-class failure mode, so make it
            // diagnosable in paneflow-debug.log. Latched to log ONCE: this runs
            // on the periodic scan, and a per-tick warn would be the very noise
            // the `cwd_now(pid=0)` fix removed.
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::warn!(
                    "macos process enumeration failed (pids_by_type: {e}) - port \
                     badges and agent detection will be unavailable"
                );
            }
            return children_of;
        }
    };
    for pid in pids {
        if pid == 0 {
            continue;
        }
        if let Ok(info) = pidinfo::<BSDInfo>(pid as i32, 0) {
            children_of.entry(info.pbi_ppid).or_default().push(pid);
        }
    }
    children_of
}

/// macOS descendant walker - BFS over the prebuilt `children_of` ppid map
/// (see [`macos_children_map`]). Walks children breadth-first from the
/// `/proc/{pid}/task/{pid}/children` traversal; `visited` is shared across
/// roots in one pass. Returns pids in BFS order.
#[cfg(target_os = "macos")]
fn bfs_descendants_macos(
    root_pid: u32,
    children_of: &std::collections::HashMap<u32, Vec<u32>>,
    visited: &mut std::collections::HashSet<u32>,
) -> Vec<u32> {
    let mut result = Vec::new();
    if !visited.insert(root_pid) {
        return result;
    }
    result.push(root_pid);
    let mut queue = std::collections::VecDeque::from([root_pid]);

    while let Some(pid) = queue.pop_front() {
        if result.len() >= MAX_PIDS_PER_ROOT {
            break;
        }
        if let Some(kids) = children_of.get(&pid) {
            for &child in kids {
                if visited.insert(child) {
                    result.push(child);
                    queue.push_back(child);
                }
            }
        }
    }

    result
}

/// macOS LISTEN ports for one PID, appended to `ports`.
///
/// Walks the PID's file descriptors via `libproc::listpidinfo::<ListFDs>`,
/// queries `pidfdinfo::<SocketFDInfo>` for every Socket FD, and filters to
/// TCP sockets in the `Listen` state. `insi_lport` in `TcpSockInfo.tcpsi_ini`
/// is the kernel's inpcb local port cast to `c_int`; the low 16 bits hold
/// the network-byte-order u16, so we mask + `from_be` to get host order.
#[cfg(target_os = "macos")]
fn listen_ports_of(pid: u32, ports: &mut Vec<u16>) {
    use libproc::libproc::file_info::{ListFDs, ProcFDType, pidfdinfo};
    use libproc::libproc::net_info::{SocketFDInfo, SocketInfoKind, TcpSIState};
    use libproc::libproc::proc_pid::listpidinfo;

    // Typical ulimit default on macOS is 256-4096 FDs per process. 1024 is
    // a sensible over-provisioning ceiling - the buffer is uninitialised
    // memory so allocation cost is a single malloc, not a zeroing pass.
    const MAX_FDS_PER_PROC: usize = 1024;

    let Ok(fds) = listpidinfo::<ListFDs>(pid as i32, MAX_FDS_PER_PROC) else {
        // EPERM / dead-process races / SIP-restricted targets → skip
        // silently. `listpidinfo` already wraps the error string, which is
        // more noise than signal during routine UI-triggered scans.
        return;
    };

    for fd in fds {
        if !matches!(ProcFDType::from(fd.proc_fdtype), ProcFDType::Socket) {
            continue;
        }

        let Ok(sfi) = pidfdinfo::<SocketFDInfo>(pid as i32, fd.proc_fd) else {
            continue;
        };

        if sfi.psi.soi_kind != SocketInfoKind::Tcp as libc::c_int {
            continue;
        }

        // SAFETY: when `soi_kind == Tcp`, the kernel guarantees the
        // `soi_proto` union's `pri_tcp` arm is the active one. The union is
        // POD (`SocketInfoProto` holds `#[repr(C)]` structs all the way
        // down) so reading a different arm would only produce garbage port
        // bytes, not UB - but we gate on `soi_kind` to keep the data
        // meaningful.
        let tcp = unsafe { sfi.psi.soi_proto.pri_tcp };

        if TcpSIState::from(tcp.tcpsi_state) as i32 != TcpSIState::Listen as i32 {
            continue;
        }

        let net_port = (tcp.tcpsi_ini.insi_lport as u32 & 0xFFFF) as u16;
        let port = u16::from_be(net_port);
        if port != 0 {
            ports.push(port);
        }
    }
}

/// argv of a pid via `sysctl(KERN_PROCARGS2)` - the macOS source for
/// `/proc/{pid}/cmdline`. EPERM (other-user pids, SIP-protected targets) and
/// malformed buffers degrade to an empty vec: the port then simply stays
/// unclassified rather than guessed at.
#[cfg(target_os = "macos")]
fn argv_of_macos(pid: u32) -> Vec<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];

    let mut size: libc::size_t = 0;
    // SAFETY: standard 3-int MIB size probe - a null buffer with a size
    // out-param is the documented sysctl(3) calling convention; nothing is
    // written besides `size`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return Vec::new();
    }

    // The probed size covers the full argv+env block, bounded by the
    // kernel's ARG_MAX (1 MiB) - a transient allocation on the unblock
    // thread, freed before the scan returns.
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` provides exactly `size` writable bytes; the kernel
    // writes at most `size` and updates it to the written length.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    buf.truncate(size);
    parse_procargs2(&buf)
}

/// Pure parser for the `KERN_PROCARGS2` buffer layout: `argc: c_int`, the
/// NUL-terminated exec path, a NUL padding run, then `argc` NUL-separated
/// argv strings (env vars follow and are ignored). Platform-neutral so the
/// fixture test runs on every host.
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buf: &[u8]) -> Vec<String> {
    let Some(argc_bytes) = buf.get(..4) else {
        return Vec::new();
    };
    let argc = i32::from_ne_bytes([argc_bytes[0], argc_bytes[1], argc_bytes[2], argc_bytes[3]])
        .max(0) as usize;
    if argc == 0 {
        return Vec::new();
    }
    let rest = &buf[4..];
    // Skip the exec path, then its NUL padding run.
    let path_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    let args_start = rest[path_end..]
        .iter()
        .position(|&b| b != 0)
        .map(|off| path_end + off)
        .unwrap_or(rest.len());
    rest[args_start..]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .take(argc)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[cfg(target_os = "macos")]
fn macos_representative_command(pids: &[u32]) -> Option<String> {
    use libproc::libproc::proc_pid::name;

    let pid = pids.last().copied()?;
    name(pid as i32)
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Scan every terminal's PTY subtree in one pass (macOS). libproc's socket
/// queries are naturally per-pid, so per-subtree attribution falls out of
/// the BFS partition without a global socket table. Same shared-`visited` /
/// single-walk contract throughout.
#[cfg(target_os = "macos")]
pub fn scan_panes(
    roots: &[(u64, u32)],
    agent_binaries: &[&str],
) -> std::collections::HashMap<u64, PaneScan> {
    use libproc::libproc::proc_pid::name;

    let mut results: std::collections::HashMap<u64, PaneScan> = std::collections::HashMap::new();
    if roots.is_empty() {
        return results;
    }

    // One ppid→children snapshot for the whole scan - every root's subtree is
    // carved out of it, so the full `listpids` enumeration happens once, not
    // per pane.
    let children_of = macos_children_map();
    let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for &(key, root_pid) in roots {
        if root_pid == 0 {
            continue;
        }
        let pids = bfs_descendants_macos(root_pid, &children_of, &mut visited);

        // `libproc::name` returns the kernel's `p_comm` - same semantics
        // and 16-char limit as a process `comm` field. EPERM (sandbox /
        // SIP) skips silently.
        let comms: Vec<String> = if agent_binaries.is_empty() {
            Vec::new()
        } else {
            pids.iter()
                .filter_map(|&pid| name(pid as i32).ok().map(|n| n.trim().to_string()))
                .collect()
        };
        let agents = agents_in_bfs_order(comms.iter().map(String::as_str), agent_binaries);

        let mut ports: Vec<PortEntry> = Vec::new();
        for &pid in &pids {
            let mut pid_ports: Vec<u16> = Vec::new();
            listen_ports_of(pid, &mut pid_ports);
            if pid_ports.is_empty() {
                continue;
            }
            // argv fetched only for pids that actually own a LISTEN socket.
            let args = argv_of_macos(pid);
            let frontend = classify_frontend_argv(args.iter().map(String::as_str));
            ports.extend(
                pid_ports
                    .into_iter()
                    .map(|port| PortEntry { port, frontend }),
            );
        }
        // Dual-stack v4+v6 binds yield two sockets on one port - keep one
        // entry, preferring a classified one.
        ports.sort_by_key(|e| (e.port, e.frontend.is_none()));
        ports.dedup_by_key(|e| e.port);

        results.insert(
            key,
            PaneScan {
                ports,
                agents,
                foreground_command: macos_representative_command(&pids),
            },
        );
    }

    results
}

// ---------------------------------------------------------------------------
// Stub (BSDs / other targets)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests (platform-neutral helpers)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // US-013 AC: "plusieurs binaires agents dans le même sous-arbre → le
    // plus proche de la racine" - first-seen BFS order wins, duplicates
    // collapse to the first occurrence.
    #[test]
    fn agents_in_bfs_order_picks_nearest_root_first_and_dedups() {
        let comms = ["zsh", "claude", "node", "codex", "claude"];
        let agents = agents_in_bfs_order(comms.into_iter(), &["claude", "codex", "opencode"]);
        assert_eq!(agents, vec!["claude".to_string(), "codex".to_string()]);
    }

    #[test]
    fn agents_in_bfs_order_exact_match_only() {
        // A wrapper named `claude-code-cli` must not trigger (exact
        // basename contract, parity with the old AI_PROCESS_NAMES match).
        let comms = ["claude-code-cli", "Claude", "claudex"];
        assert!(agents_in_bfs_order(comms.into_iter(), &["claude"]).is_empty());
    }

    #[test]
    fn agents_in_bfs_order_empty_inputs() {
        assert!(agents_in_bfs_order(std::iter::empty(), &["claude"]).is_empty());
        assert!(agents_in_bfs_order(["claude"].into_iter(), &[]).is_empty());
    }

    #[test]
    fn classify_frontend_argv_matches_basenames_and_titles() {
        // node running the .bin shim - the canonical vite/next launch shape.
        let argv = ["node", "/repo/node_modules/.bin/vite"];
        assert_eq!(classify_frontend_argv(argv.into_iter()), Some("Vite"));
        // bun executing the package bin JS directly.
        let argv = ["bun", "/repo/node_modules/vite/bin/vite.js"];
        assert_eq!(classify_frontend_argv(argv.into_iter()), Some("Vite"));
        // Next.js rewrites its process title to one "next-server (vX)" token.
        let argv = ["next-server (v15.3.2)"];
        assert_eq!(classify_frontend_argv(argv.into_iter()), Some("Next.js"));
        let argv = ["node", "/repo/node_modules/.bin/next", "dev"];
        assert_eq!(classify_frontend_argv(argv.into_iter()), Some("Next.js"));
        let argv = ["node", "/usr/lib/node_modules/@angular/cli/bin/ng", "serve"];
        assert_eq!(classify_frontend_argv(argv.into_iter()), Some("Angular"));
    }

    #[test]
    fn classify_frontend_argv_rejects_lookalikes() {
        // Basename matching, not substring: a path that merely CONTAINS a
        // framework name must not arm a clickable chip.
        let argv = ["node", "/srv/invite/server.js"];
        assert_eq!(classify_frontend_argv(argv.into_iter()), None);
        let argv = ["node", "/srv/vitesse-app/index.js"];
        assert_eq!(classify_frontend_argv(argv.into_iter()), None);
        let argv = ["python3", "-m", "http.server"];
        assert_eq!(classify_frontend_argv(argv.into_iter()), None);
        assert_eq!(classify_frontend_argv(std::iter::empty()), None);
    }

    #[test]
    fn normalize_process_basename_strips_common_windows_wrappers() {
        assert_eq!(normalize_process_basename(r"C:\tools\codex.exe"), "codex");
        assert_eq!(normalize_process_basename("vite.CMD"), "vite");
        assert_eq!(normalize_process_basename("vite.cmd"), "vite");
        assert_eq!(normalize_process_basename("script.ps1"), "script");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scan_panes_detects_current_process_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let scan = scan_panes(&[(1, std::process::id())], &[]);
        let ports = scan
            .get(&1)
            .map(|s| s.ports.iter().map(|e| e.port).collect::<Vec<_>>())
            .unwrap_or_default();

        assert!(
            ports.contains(&port),
            "scan_panes must detect a live listener owned by the root pid; got {ports:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scan_panes_ignores_pid_zero_roots() {
        let scan = scan_panes(&[(1, 0)], &[]);
        assert!(
            scan.is_empty(),
            "pid 0 is a display-only sentinel and must not scan the system tree"
        );
    }

    #[test]
    fn parse_procargs2_extracts_argv_after_exec_path() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&2i32.to_ne_bytes());
        // Exec path + NUL padding run, then argc args, then env (ignored).
        buf.extend_from_slice(b"/usr/local/bin/node\0\0\0\0");
        buf.extend_from_slice(b"node\0/repo/node_modules/.bin/vite\0");
        buf.extend_from_slice(b"PATH=/usr/bin\0");
        assert_eq!(
            parse_procargs2(&buf),
            vec![
                "node".to_string(),
                "/repo/node_modules/.bin/vite".to_string()
            ]
        );
        assert!(parse_procargs2(&[]).is_empty());
        assert!(parse_procargs2(&[1, 0, 0]).is_empty());
        assert!(parse_procargs2(&0i32.to_ne_bytes()).is_empty());
    }

    // Regression for the workspace-card blue dot on Apple: the macOS subtree
    // scan must find a live PTY descendant and resolve its `p_comm`. This
    // failed silently while the walk relied on `proc_listchildpids`, which
    // returns 0 children for an unprivileged caller - `detected_agents` stayed
    // empty and the dot never lit. We spawn the real, signed `/bin/sleep`
    // (p_comm == "sleep"; no code-signing confound) as a child of the test
    // process and assert it surfaces.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_scan_panes_detects_live_child_subtree() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        // Let the kernel register the new process's BSD info before we probe.
        std::thread::sleep(std::time::Duration::from_millis(250));

        let roots = [(1u64, std::process::id())];
        let scan = scan_panes(&roots, &["sleep"]);

        let _ = child.kill();
        let _ = child.wait();

        let agents = scan.get(&1).map(|s| s.agents.clone()).unwrap_or_default();
        assert!(
            agents.iter().any(|a| a == "sleep"),
            "macOS subtree scan must detect the live `sleep` child; got {agents:?}"
        );
    }
}
