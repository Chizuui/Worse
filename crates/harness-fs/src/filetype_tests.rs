#[cfg(test)]
mod cases {
    use crate::filetype::*;

    fn pe_bytes() -> Vec<u8> {
        let mut bytes = vec![0_u8; 96];
        bytes[0] = b'M';
        bytes[1] = b'Z';
        bytes[0x3c..0x40].copy_from_slice(&64_u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes
    }

    #[test]
    fn pe_named_text_warns() {
        let path = Path::new("/w/notes.txt");
        assert_eq!(classify_magic(&pe_bytes()), ContentKind::Pe);
        let found = report(path, classify_magic(&pe_bytes()));
        assert_eq!(found.label, "Windows executable");
        assert_eq!(
            found.warning.as_deref(),
            Some("Content looks like: Windows executable")
        );
    }

    #[test]
    fn text_named_text_does_not_warn() {
        let bytes = b"hello world, this is a plain text file\n";
        assert_eq!(classify_magic(bytes), ContentKind::Text);
        assert!(report(Path::new("/w/notes.txt"), ContentKind::Text)
            .warning
            .is_none());
    }

    #[test]
    fn short_files_are_unknown() {
        assert_eq!(classify_magic(b"MZ"), ContentKind::Unknown);
        assert!(report(Path::new("/w/a.txt"), ContentKind::Unknown)
            .warning
            .is_none());
    }

    #[test]
    fn script_named_image_warns() {
        let bytes = b"#!/bin/sh\necho hello\n";
        assert_eq!(classify_magic(bytes), ContentKind::Script);
        assert!(report(Path::new("/w/logo.png"), ContentKind::Script)
            .warning
            .is_some());
        let found = report(Path::new("/w/run.sh"), ContentKind::Script);
        assert_eq!(found.warning, None);
        assert_eq!(
            report(Path::new("/w/run.ps1"), ContentKind::Script).warning,
            None
        );
    }

    #[test]
    fn png_named_text_warns() {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&[0_u8; 32]);
        assert_eq!(classify_magic(&bytes), ContentKind::Image);
        assert!(report(Path::new("/w/readme.txt"), ContentKind::Image)
            .warning
            .is_some());
        assert!(report(Path::new("/w/logo.png"), ContentKind::Image)
            .warning
            .is_none());
    }

    #[test]
    fn elf_without_extension_warns() {
        let mut bytes = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0];
        bytes.extend_from_slice(&[0_u8; 16]);
        assert_eq!(classify_magic(&bytes), ContentKind::Elf);
        assert!(report(Path::new("/w/tool"), ContentKind::Elf)
            .warning
            .is_some());
        assert_eq!(
            report(Path::new("/w/tool"), ContentKind::Elf).claimed,
            "none"
        );
    }

    #[test]
    fn claimed_groups() {
        assert_eq!(claimed_group(Path::new("a.rs")), Claimed::Text);
        assert_eq!(claimed_group(Path::new("a.EXE")), Claimed::Executable);
        assert_eq!(claimed_group(Path::new("a.zip")), Claimed::Other);
        assert_eq!(claimed_group(Path::new("Makefile")), Claimed::None);
    }

    #[test]
    fn cache_reuses_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, pe_bytes()).unwrap();
        let cache = ContentCache::new();
        let first = cache.inspect(&path);
        assert_eq!(first.kind, ContentKind::Pe);
        assert_eq!(cache.len(), 1);
        let second = cache.inspect(&path);
        assert_eq!(first, second);
        std::fs::write(&path, b"now plain text\n").unwrap();
        assert_eq!(cache.inspect(&path).kind, ContentKind::Text);
    }
}
