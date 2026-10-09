# Notes for upstream contributions

This crate reads git's index and applies gitignore rules from git's
documentation. Where the documentation and git disagree, the crate follows git.
The items below are candidates for patches to git's documentation, or for a
bug report. Each was observed with git 2.55.0 on Linux in a fresh repository,
with `HOME` pointing at an empty directory and `GIT_CONFIG_NOSYSTEM=1`.

## Documentation/gitformat-index.adoc

### Untracked cache: the stat data array follows the valid bitmap

The documentation says the array of stat data has one entry per set bit of
the third bitmap, the one that marks directories with a valid hash:

> An array of stat data. The n-th data corresponds with the n-th "one" bit in
> the previous ewah bitmap.

Git writes one 36-byte stat entry per set bit of the first bitmap, the one
that marks directories with valid untracked cache entries. In a repository
with tracked `c/.gitignore` and untracked files in `a/`, `c/` and `d/e/`, the
cache has 5 valid directories and 1 directory with a hash. The bytes after the
bitmaps are 201 = 5 × 36 + 1 × 20 + 1.

### Untracked cache: no NUL after a zero directory count

The documentation says:

> If this number is zero, the extension ends here with a following NUL.

After `git update-index --untracked-cache` in an empty repository, the
extension ends directly after the zero count.

### Untracked cache: which variable width encoding

The untracked cache section says "variable width encoding" for its counts
without naming the encoding. Git uses the offset encoding of OFS_DELTA pack
entries, which the version 4 path compression section names explicitly. A
directory with 200 untracked files stores its count as `80 48`, which is 200
in that encoding.

### Untracked cache: hashes of the exclude files

The documentation calls the two header hashes "Hash of
$GIT_COMMON_DIR/info/exclude" and "Hash of core.excludesFile". Git records:

- the null hash when the file doesn't exist,
- the empty blob's hash when the file is empty,
- otherwise the blob hash of the contents followed by an extra `\n`. For an
  exclude file containing `foo` without a newline, git records
  `257cc5642cb1a054f08cc83f2d943e56fd3ebe99`, which is
  `printf 'foo\n' | git hash-object --stdin`.

### File System Monitor cache

- The documentation lists versions 1 and 2. Git writes version 2 for hook
  protocol version 1 as well, with the token holding the time in nanoseconds
  as a decimal string.
- "32-bit bitmap size: the size of the CE_FSMONITOR_VALID bitmap" is the
  length of the serialized EWAH bitmap in bytes (20 for an empty bitmap).
- The bitmap's bit count ends at its last set bit. With no dirty entries it
  has 0 bits, whatever the number of index entries.

## Documentation/technical/bitmap-format.adoc

### EWAH run length counts words

Appendix A describes a run length word as "32 bits: repetition count K" and
the chunk as "K repetitions of B", which reads as K bits. K counts 64-bit
words. An untracked cache with 201 valid directories stores its valid bitmap
as a run length word with B = 1, K = 3, M = 1: three words of ones and one
literal word.

## Documentation/gitignore.adoc

The documentation refers to fnmatch(3) with FNM_PATHNAME. Git matches
differently from glibc's fnmatch(3) in these cases:

| Pattern         | Path    | git      | glibc fnmatch(3) |
| --------------- | ------- | -------- | ---------------- |
| `a[b`           | `a[b`   | no match | match            |
| `[c-a]`         | `c`     | match    | no match         |
| `***/a`         | `x/y/a` | match    | no match         |
| `x[[:space:]]y` | `x\vy`  | no match | match            |
| `x[[:space:]]y` | `x\fy`  | no match | match            |

- An unterminated `[` makes the pattern invalid, and an invalid pattern never
  matches. The documentation says this only for a trailing backslash.
- A reversed range matches its first character only.
- The documentation says "Other consecutive asterisks are considered regular
  asterisks". A run of three or more asterisks between slashes acts like `**`:
  `***/a` matches `a`, `x/a` and `x/y/a`.
- `[:space:]` matches space, `\t`, `\n` and `\r`, without `\v` and `\f`.
- An escaped slash `\/` acts as a separator: `a\/b` matches `a/b` and not
  `x/a/b`.

Reproduce with a pattern in `.gitignore` and
`git check-ignore --no-index -v -n -- <path>`.

## Possible bug: core.ignoreCase and escaped or bracketed letters

With `core.ignoreCase=true`, the pattern `A` matches both `A` and `a`, but
`[A]` and `\A` match neither:

```sh
git init -q repro && cd repro
printf '[A]\n' > .gitignore
git -c core.ignoreCase=true check-ignore --no-index -v -n -- A a
# both paths are reported as not ignored
printf 'A\n' > .gitignore
git -c core.ignoreCase=true check-ignore --no-index -v -n -- A a
# both paths are ignored by .gitignore:1:A
```

An uppercase letter written as an escape or a one-letter bracket expression
therefore never matches on a case-insensitive file system, where a plain
uppercase letter does.
