use std::{fs, io::Write, process::Command};

use mldo::{
    input::{parse_links, parse_structure},
    mapping::{build_jobs, suggest},
    staging::{self, Coordinator, Outcome},
};

#[test]
fn executable_reprocesses_only_registered_completes_without_network_or_gui() {
    let root = tempfile::tempdir().unwrap();
    let schema = "Game/a.bin";
    let structure = parse_structure(schema).unwrap();
    let links = parse_links("https://vikingfile.com/f/not-a-real-file").unwrap();
    let jobs = build_jobs(&links, &structure, &suggest(&links, &structure)).unwrap();
    let directory = staging::directory(root.path()).unwrap();
    let mut file = tempfile::Builder::new()
        .suffix(".download")
        .tempfile_in(&directory)
        .unwrap();
    file.write_all(b"data").unwrap();
    file.as_file_mut().sync_all().unwrap();
    fs::write(root.path().join(staging::NOT_IDENTIFY), b"blocker").unwrap();
    assert!(matches!(
        Coordinator::new(structure)
            .commit(root.path(), &jobs[0], file, 4, Some("unknown.bin".into()))
            .unwrap(),
        Outcome::Pending(_)
    ));
    fs::remove_file(root.path().join(staging::NOT_IDENTIFY)).unwrap();
    fs::write(directory.join("partial.download"), b"partial").unwrap();
    let schema_path = root.path().join("schema.txt");
    fs::write(&schema_path, schema).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_mldo"))
        .arg("--organize")
        .arg("--schema")
        .arg(&schema_path)
        .arg("--destination")
        .arg(root.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read(root.path().join("NotIdentify/unknown.bin")).unwrap(),
        b"data"
    );
    assert_eq!(
        fs::read(directory.join("partial.download")).unwrap(),
        b"partial"
    );
    let report = fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "log"))
        .unwrap();
    assert!(fs::read_to_string(report).unwrap().contains("NotIdentify"));
    let status = Command::new(env!("CARGO_BIN_EXE_mldo"))
        .arg("--organize")
        .arg("--execute")
        .arg("bad.ps1")
        .status()
        .unwrap();
    assert!(!status.success());
}
