fn get_platform_tag(system: &str, machine: &str) -> String {
    match (system, machine) {
        ("Darwin", "arm64") => "macosx_11_0_arm64".to_string(),
        ("Darwin", "x86_64") => "macosx_11_0_x86_64".to_string(),
        ("Linux", "x86_64" | "amd64") => "manylinux_2_17_x86_64".to_string(),
        ("Linux", "aarch64" | "arm64") => "manylinux_2_17_aarch64".to_string(),
        ("Windows", "AMD64" | "x86_64") => "win_amd64".to_string(),
        ("Windows", "ARM64") => "win_arm64".to_string(),
        _ => format!("{}_{}", system.to_lowercase(), machine),
    }
}

#[test]
fn platform_tags_match_python_build_wheel_script() {
    for (system, machine, expected) in [
        ("Darwin", "arm64", "macosx_11_0_arm64"),
        ("Darwin", "x86_64", "macosx_11_0_x86_64"),
        ("Linux", "x86_64", "manylinux_2_17_x86_64"),
        ("Linux", "amd64", "manylinux_2_17_x86_64"),
        ("Linux", "aarch64", "manylinux_2_17_aarch64"),
        ("Linux", "arm64", "manylinux_2_17_aarch64"),
        ("Windows", "AMD64", "win_amd64"),
        ("Windows", "x86_64", "win_amd64"),
        ("Windows", "ARM64", "win_arm64"),
    ] {
        assert_eq!(get_platform_tag(system, machine), expected);
    }
}

#[test]
fn unknown_platform_tags_fall_through_like_python_build_wheel_script() {
    assert_eq!(get_platform_tag("Linux", "riscv64"), "linux_riscv64");
    assert_eq!(get_platform_tag("FreeBSD", "amd64"), "freebsd_amd64");
}
