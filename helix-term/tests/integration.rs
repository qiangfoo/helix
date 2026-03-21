#[cfg(feature = "integration")]
mod test {
    mod helpers;

    use helix_core::Selection;
    use helix_term::config::Config;

    use indoc::indoc;

    use self::helpers::*;

    mod auto_indent;
    mod command_line;
    mod commands;
    mod languages;
    mod movement;
    mod splits;
}
