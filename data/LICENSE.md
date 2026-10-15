# Bundled dictionary data license

The compiled dictionaries in this directory (`dict-ja.bin`, `dict-zh.bin`)
are derived from the datasets below. They are licensed under
[Creative Commons Attribution-ShareAlike 4.0 International][ccbysa]
(CC BY-SA 4.0), as are the word rules compiled from them. The `rubify`
source code is licensed separately (Apache-2.0 OR MIT).

`cargo xtask update-dict` downloads and compiles the sources. Each blob
records the source versions it was built from.

## `dict-ja.bin` (Japanese)

- [furifont](https://github.com/antfu/furifont) by Anthony Fu, the compiler
  whose word list, readings and curated overrides this data uses.
- [JMdict](https://www.edrdg.org/jmdict/j_jmdict.html) and
  [JMnedict](https://www.edrdg.org/enamdict/enamdict_doc.html), property of
  the [Electronic Dictionary Research and Development Group](https://www.edrdg.org/),
  used in conformance with the Group's [licence](https://www.edrdg.org/edrdg/licence.html).
- [KANJIDIC2](https://www.edrdg.org/wiki/index.php/KANJIDIC_Project) (EDRDG),
  for on'yomi data.
- [JmdictFurigana](https://github.com/Doublevil/JmdictFurigana) by Doublevil,
  which aligns readings to characters.
- [yomitan-jlpt-vocab](https://github.com/stephenmk/yomitan-jlpt-vocab) by
  stephenmk, for JLPT levels: Jonathan Waller's JLPT lists, tied to JMdict.
- The [Tatoeba](https://tatoeba.org/) project's Japanese sentences and their
  JMdict index, as distributed by the EDRDG, under
  [CC BY 2.0 FR](https://creativecommons.org/licenses/by/2.0/fr/).

## `dict-zh.bin` (Chinese)

- [CC-CEDICT](https://cc-cedict.org/), the community maintained
  Chinese-English dictionary, published by MDBG, under
  [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/).

[ccbysa]: https://creativecommons.org/licenses/by-sa/4.0/
