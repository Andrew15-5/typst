default: run

compile_name := "title"

alias e := edit
edit:
  "$EDITOR" crates/typst-cli/src/watch.rs

alias b := build
build *args:
  cargo build --bin typst {{args}}

alias r := run
run:
  # rm -rf ~/.cache/typst/packages/preview/pyrunner
  cargo run --bin typst -- compile '{{compile_name}}.typ'
  # ./target/release/typst compile '{{compile_name}}.typ'

alias w := watch
watch:
  cargo run --bin typst -- watch --root ./tmp ./tmp/a.typ
