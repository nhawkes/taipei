//! The intro's full example is the crate's `hello` example, verbatim: the fence the reader
//! sees is the file cargo compiles.

#[test]
fn the_intro_fence_is_the_hello_example() {
    let intro = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/intro.md")).unwrap();
    let fence = intro
        .split("```rust\n")
        .nth(1)
        .and_then(|rest| rest.split("\n```").next())
        .expect("the intro opens with a rust fence");
    let example = include_str!("../../taipei/examples/hello.rs");
    assert_eq!(fence, example.trim_end_matches('\n'));
}
