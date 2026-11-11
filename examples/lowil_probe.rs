//! S0 spike for the built-in shell: prove (or disprove) the three facts the boundary rests on.
//! Run it with `cargo run --example lowil_probe`. It uses `%TEMP%` only and removes what it created.
//! What it establishes, in order.

use std::path::Path;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--il" {
        match aox::shell::lowil::current_il_attrs() {
            Ok((rid, attrs)) => {
                println!("IL=0x{rid:X} ({}) attrs=0x{attrs:X}", aox::shell::lowil::il_name(rid));
                std::process::exit(0);
            }
            Err(e) => {
                println!("IL=? ({e})");
                std::process::exit(1);
            }
        }
    }
    if args.len() == 3 && args[1] == "--write" {
        // Used as a *grandchild* of the helper: it only reports whether an inherited Low token can
        // still create a file where the workspace says it should be able to.
        let target = Path::new(&args[2]).join("written-by-inherited.txt");
        match std::fs::write(&target, "x") {
            Ok(()) => {
                println!("WRITE-OK {}", target.display());
                std::process::exit(0);
            }
            Err(e) => {
                println!("WRITE-FAIL {e}");
                std::process::exit(1);
            }
        }
    }
    if args.len() == 5 && args[1] == "--child" {
        std::process::exit(child(Path::new(&args[2]), Path::new(&args[3]), Path::new(&args[4])));
    }
    std::process::exit(parent());
}

// the Medium side

fn parent() -> i32 {
    let root = std::env::temp_dir().join(format!("aox-lowil-spike-{}", std::process::id()));
    let ws = root.join("ws");
    let outside = root.join("outside-medium.txt");
    let _ = std::fs::remove_dir_all(&root);
    if let Err(e) = std::fs::create_dir_all(&ws) {
        println!("FAIL  could not create the probe tree: {e}");
        return 1;
    }
    std::fs::write(ws.join("pre-existing.txt"), "written by the host before labelling\n").unwrap();
    std::fs::write(&outside, "a Medium file outside the workspace\n").unwrap();

    println!("\n=== S0 spike: low-integrity shell child ===\n");
    println!("workspace  {}", ws.display());
    println!("outside    {}", outside.display());
    println!(
        "host IL    {}",
        aox::shell::lowil::current_il()
            .map(|r| format!("0x{r:X} ({})", aox::shell::lowil::il_name(r)))
            .unwrap_or_else(|e| format!("<{e}>"))
    );

    // ① labelling from an unprivileged Medium process
    match aox::shell::lowil::label_low(&ws) {
        Ok(()) => println!("PASS  icacls moved the tree to Low (no elevation needed)"),
        Err(e) => {
            println!("FAIL  could not label the tree: {e}");
            println!("      => boundary must degrade to \"scan\"");
            let _ = std::fs::remove_dir_all(&root);
            return 1;
        }
    }

    // ②③④ inside the child
    let exe = std::env::current_exe().expect("current_exe");
    let out = Command::new(&exe)
        .arg("--child")
        .arg(&ws)
        .arg(&outside)
        .arg(&root)
        .output()
        .expect("spawn the child");
    let text = String::from_utf8_lossy(&out.stdout);
    print!("{text}");
    if !out.stderr.is_empty() {
        println!("--- child stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
    }
    let mut failed = text.lines().filter(|l| l.starts_with("FAIL")).count();
    if !out.status.success() {
        println!("FAIL  the child exited with {:?}", out.status.code());
        failed += 1;
    }

    // ⑤ the host keeps writing its own workspace
    match std::fs::write(ws.join("after-label-by-host.txt"), "host still writes here\n") {
        Ok(()) => println!("PASS  the Medium host can still write the Low workspace"),
        Err(e) => {
            println!("FAIL  the host lost write access to its own workspace: {e}");
            failed += 1;
        }
    }

    let _ = std::fs::remove_dir_all(&root);
    println!(
        "\n=== {} ===\n",
        if failed == 0 { "all checks passed" } else { "there are failures above — do not build on this" }
    );
    failed as i32
}

// the (soon to be) Low side

fn child(ws: &Path, outside: &Path, root: &Path) -> i32 {
    use aox::shell::lowil;
    let mut failed = 0;

    match lowil::current_il_attrs() {
        Ok((rid, attrs)) => println!("      child starts at 0x{rid:X} ({}) attrs=0x{attrs:X}", lowil::il_name(rid)),
        Err(e) => {
            println!("FAIL  could not read the child's integrity level: {e}");
            return 1;
        }
    }

    // ② lowering our own token, unprivileged
    match lowil::lower_self() {
        Ok(rid) if rid == lowil::LOW_IL_RID => {
            let attrs = lowil::current_il_attrs().map(|(_, a)| a).unwrap_or(0);
            println!(
                "PASS  the child lowered its own token to 0x{rid:X} ({}) without elevation, attrs=0x{attrs:X}",
                lowil::il_name(rid)
            )
        }
        Ok(rid) => {
            println!("FAIL  lowering produced 0x{rid:X} ({}) instead of Low", lowil::il_name(rid));
            failed += 1;
        }
        Err(e) => {
            println!("FAIL  could not lower this token: {e}");
            return 1;
        }
    }

    // The kernel agrees: ask a *grandchild* what it inherited.
    match Command::new(std::env::current_exe().expect("current_exe")).arg("--il").output() {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.contains("IL=0x1000") {
                println!("PASS  a grandchild inherits the label: it reports {}", s.trim());
            } else {
                println!("FAIL  the grandchild did not inherit Low: it reports {:?}", s);
                failed += 1;
            }
        }
        Err(e) => {
            println!("FAIL  could not spawn the grandchild: {e}");
            failed += 1;
        }
    }

    // ④a write a brand-new file inside the labelled workspace
    match std::fs::write(ws.join("new-by-child.txt"), "created by the low child\n") {
        Ok(()) => println!("PASS  a new file inside the workspace"),
        Err(e) => {
            println!("FAIL  could not create a file inside the workspace: {e}");
            failed += 1;
        }
    }

    // ④b overwrite a file that existed *before* the labelling pass
    match std::fs::OpenOptions::new().append(true).open(ws.join("pre-existing.txt")) {
        Ok(mut f) => {
            use std::io::Write;
            match f.write_all(b"appended by the low child\n") {
                Ok(()) => println!("PASS  appending to a pre-existing workspace file"),
                Err(e) => {
                    println!("FAIL  could not append to an existing workspace file: {e}");
                    failed += 1;
                }
            }
        }
        Err(e) => {
            println!("FAIL  could not open an existing workspace file for append: {e}");
            failed += 1;
        }
    }

    // ④c the point of the whole exercise: a Medium file outside the workspace
    let denied = std::fs::write(outside, "the low child should never get here\n");
    match &denied {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            println!("PASS  writing an out-of-workspace file was refused: {e}")
        }
        Err(e) => {
            println!("FAIL  the out-of-workspace write failed for the wrong reason: {e}");
            failed += 1;
        }
        Ok(()) => {
            println!("FAIL  THE BOUNDARY DOES NOT HOLD: the child wrote {}", outside.display());
            failed += 1;
        }
    }

    // ④d and the same for *creating* something in a Medium directory
    let new_outside = root.join("new-by-child-outside.txt");
    match std::fs::write(&new_outside, "x") {
        Ok(()) => {
            println!("FAIL  the child created {} outside the workspace", new_outside.display());
            failed += 1;
        }
        Err(e) => println!("PASS  creating a file outside the workspace was refused: {e}"),
    }

    // ④e reads are not restricted (the asymmetric half of the contract)
    match std::fs::read_to_string(outside) {
        Ok(s) if s.contains("Medium file outside") => {
            println!("PASS  reading outside the workspace still works")
        }
        Ok(_) => println!("PASS  reading outside the workspace works (content differs, fine)"),
        Err(e) => {
            println!("FAIL  reading outside the workspace broke: {e}");
            failed += 1;
        }
    }

    failed as i32
}
