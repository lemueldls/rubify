# Rubify

> Annotate fonts with ruby (pinyin/romaji) and produce modified TTF/WOFF2 outputs.

- Render ruby annotations using pluggable renderers (`pinyin`, `romaji`)
- Subset output fonts to only include annotation characters
- Optionally split TTC into individual TTF files
- Optional WOFF2 output (feature-flagged, currently only supported when splitting collections)

## Library usage

See the [rubify](https://docs.rs/rubify) crate documentation for API details and examples.

## CLI Usage

```sh
rubify <input-file-or-glob> -o <out-dir> --ruby <pinyin|romaji> [options]
```

- `--out, -o <path>`: Output directory (required)
- `--ruby <pinyin|romaji>`: Which annotation renderer to use (requires building with the corresponding feature)
- `--font <path>`: Separate font file to use for ruby characters
- `--subset`: Subset output font to contain only annotation characters
- `--split`: When input is a TTC, write each font as a separate TTF file instead of rebuilding a TTC
- `--woff2`: Convert outputs to WOFF2

### Example

Process a single TTC, split into TTFs, subset and output WOFF2:

```sh
rubify Sarasa-Regular.ttc -o dist --font iosevka/IosevkaSlim-Regular.ttf --subset --ruby romaji --position bottom --split --woff2
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
