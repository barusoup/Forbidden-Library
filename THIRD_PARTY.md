# Third-party components

## FFmpeg

The installer carries a Windows build of FFmpeg (the "essentials" build from
gyan.dev) as two separate executables, `ffmpeg.exe` and `ffprobe.exe`, in
`ffmpeg`. The app runs them as subprocesses for probing containers and
generating video and uncommon-image previews. FFmpeg is not linked into the app.

FFmpeg is licensed under the GNU General Public License, version 3 or later,
with portions under the GNU Lesser General Public License. The full licence
text that ships with the build is copied to `ffmpeg\LICENSE.txt` in the
installed payload and in `assets/ffmpeg/LICENSE.txt` in this repository.

Source and build information: <https://ffmpeg.org/> and
<https://www.gyan.dev/ffmpeg/builds/>.

## libjxl

The JPEG XL command-line tools `cjxl.exe` and `djxl.exe` (libjxl v0.12.0) are
bundled in `jxl` beside the app. The app runs them as subprocesses to
transcode JPEGs losslessly, to encode lossless PNGs and to rebuild a JPEG from
its JPEG XL form. libjxl is not linked into the app.

libjxl is licensed under the BSD 3-Clause License. Copyright (c) the JPEG XL
Project Authors. The licence text that ships with the tools is copied to
`jxl/LICENSE.txt` in the installed payload and in `assets/jxl/LICENSE.txt` in
this repository.

Source and release: <https://github.com/libjxl/libjxl/releases/tag/v0.12.0>.

## jxl-oxide

JPEG XL is decoded in-app by the `jxl-oxide` crate and the jxl-* crates it
depends on (version 0.12.6). They are licensed under the MIT License or the
Apache License, Version 2.0, at your option. The crate's source is
<https://github.com/tirr-c/jxl-oxide>.

## EmbeddingGemma and llama.cpp

Semantic search runs the EmbeddingGemma model (GGUF conversion from
<https://huggingface.co/unsloth/embeddinggemma-2-GGUF>) through the
`llama-server` runtime from llama.cpp (<https://github.com/ggml-org/llama.cpp>,
MIT License). Both are bundled in `semantic` and run locally as a subprocess.
The model is provided under the Gemma Terms of Use,
<https://ai.google.dev/gemma/terms>.

## Roboto

Roboto is bundled inside the app for the interface.

Copyright 2011 Google Inc. Roboto is licensed under the Apache License,
Version 2.0. <https://www.apache.org/licenses/LICENSE-2.0>

The font files live in `assets/fonts/` with their original licence text. The
licence is also installed beside the app as `Roboto-LICENSE.txt`.

## Lucide icons

The interface icons are vendored from Lucide (lucide-static 0.544.0,
<https://lucide.dev>) and rasterized from Lucide's own SVG geometry.

ISC License. Copyright (c) for portions of Lucide are held by Cole Bemis
2013-2022, and other parties (2022) as listed in the Lucide repository.

<https://github.com/lucide-icons/lucide/blob/main/LICENSE>

The import picker uses Lucide's `folder-open` and `smartphone` icons from the
same 0.544.0 release, under the ISC license above. Their original SVGs are in
`assets/icons/lucide/`; the smartphone's rounded rect was converted to an
equivalent vector path using svgelements.

The wireless upload page uses Lucide's `file-up`, `x`, `check` and
`circle-alert` icons from the same release, under the same license. Their
original SVGs are in `assets/icons/lucide/`, and the page inlines their paths.

## Rust dependencies

The complete dependency graph and versions are recorded in `Cargo.lock`.
Notable licenses include MIT and Apache-2.0 for the Rust crates, SQLite
(public domain) via `rusqlite`'s bundled build, and the image codec crates
under their respective permissive licenses.
