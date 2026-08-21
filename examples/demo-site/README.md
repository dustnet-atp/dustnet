# Example site

A small multi-page site written to exercise the markup a real one uses:
cross-page navigation, panel state machines, page transitions, live regions,
frame animations, WASM effect references, forms, tables and typography.

It exists for two reasons. It is the worked example of what AML looks like
beyond a single page, and it is corpus for
`crates/dustnet-core/tests/aml_corpus.rs`, which requires every tracked
document to scan and parse within current client limits.

This repository ships no other site content. Authored sites live in their own
repositories and version separately.

## Layout

```
index.aml           navigation hub
text.aml            typography, colour, rules, preformatted
layout.aml          borders, rows and columns, nesting, absolute placement
lists-tables.aml    bullet and numbered lists, bordered and plain tables
forms.aml           input, select, submit, focus traversal
panels.aml          panel state machines and a transitioned drawer
animations.aml      authored frame animations, including one chained on another
effects.aml         references to sandboxed WASM modules
live.aml            two live regions
status.aml          fragment served to the /status live region
feed.aml            fragment served to the /feed live region
transitions.aml     links naming a transition
transitions/*.aml   one arrival page per transition
```

## Serving it

```sh
make serve SITE_DIR=examples/demo-site
```

That expands any `{{figlet:}}` markers into a clean tree under `target/site`,
validates every emitted page with `dustnet check`, and serves it on port 1985
over plaintext loopback. Then, in another terminal:

```sh
make client
```

## The WASM effects

`effects.aml` references `/effects/*.wasm`, which are not tracked here — they
are build outputs of the crates under `effects/`. To see them:

```sh
make effects
mkdir -p examples/demo-site/effects
cp effects/procedural_backgrounds/target-starfield/wasm32-unknown-unknown/release/procedural_backgrounds.wasm \
   examples/demo-site/effects/starfield.wasm
cp effects/static_noise/target/wasm32-unknown-unknown/release/static_noise.wasm \
   examples/demo-site/effects/static_noise.wasm
```

A page whose effect module is missing still loads: the animation is skipped and
the rest of the page renders. That is deliberate — a failed effect must never
take the page with it.
