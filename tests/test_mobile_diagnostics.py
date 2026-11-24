"""Exercise the real iOS bounded log writer without building the renderer."""
import pathlib, subprocess, tempfile, unittest
import shutil
ROOT = pathlib.Path(__file__).resolve().parents[1]
class MobileDiagnosticsTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("cargo"), "requires Cargo")
    def test_rotation_and_redaction(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            source = '#[path="' + str(ROOT / "crates/horizon-client-render/src/mobile_log.rs") + '"] mod mobile_log;\n' + SOURCE
            (root / "src").mkdir()
            (root / "src/lib.rs").write_text(source)
            (root / "Cargo.toml").write_text(
                '[package]\nname="horizon-mobile-log-fixture"\nversion="0.0.0"\nedition="2024"\n'
                '[dependencies]\ntempfile="3"\n'
            )
            subprocess.run(["cargo", "test", "--quiet", "--manifest-path", str(root / "Cargo.toml")],
                           check=True, timeout=120)
SOURCE = r'''
#[test] fn privacy_and_rotation() {
 use std::{fs,io::{Read,Seek,SeekFrom}};
 let d=std::env::temp_dir().join(format!("horizon-log-test-{}",std::process::id()));fs::create_dir_all(&d).unwrap();
 let mut f=fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(d.join("latest.log")).unwrap();
 for secret in ["Authorization: Bearer abc", "{\"refresh_token\":\n\"short-secret\"}","XBL3.0 x=secret"] {assert!(!mobile_log::redact(secret).contains("secret"));}
 assert!(mobile_log::redact("shader compilation: unexpected token near main()").contains("unexpected token"));
 for _ in 0..90 {mobile_log::append(&mut f,&d,&format!("[12:00:00] [Render thread/WARN]: {}", "shader failure details ".repeat(1000))).unwrap();}
 assert!(f.metadata().unwrap().len() <= mobile_log::LIMIT as u64);
 assert!(fs::metadata(d.join("previous.log")).unwrap().len() <= mobile_log::LIMIT as u64);
 mobile_log::append(&mut f,&d,"access_token=never-export-this").unwrap();f.seek(SeekFrom::Start(0)).unwrap();let mut s=String::new();f.read_to_string(&mut s).unwrap();assert!(!s.contains("never-export-this"));fs::remove_dir_all(d).unwrap();
}
'''
