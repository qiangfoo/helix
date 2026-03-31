#[cfg(feature = "integration")]
mod test {
    mod helpers;

    use helix_core::Selection;

    use indoc::indoc;

    use self::helpers::*;

    mod command_line;
    mod commands;
    mod movement;
    mod tabs;
}
