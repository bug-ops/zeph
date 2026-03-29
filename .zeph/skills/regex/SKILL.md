---
name: regex
category: data
description: >
  Write, test, and debug regular expressions for pattern matching, search, and
  text extraction across programming languages. Use when the user asks to match
  patterns in text, validate input formats (email, URL, IP, phone, date),
  extract data with capture groups, build search patterns with lookahead or
  lookbehind, or understand regex syntax differences between PCRE, POSIX,
  JavaScript, Python, and Rust flavors.
license: MIT
metadata:
  author: zeph
  version: "1.0"
---

# Regular Expressions Reference

## Quick Reference

| Pattern | Matches |
|---------|---------|
| `.` | Any character (except newline) |
| `\d` | Digit [0-9] |
| `\w` | Word character [a-zA-Z0-9_] |
| `\s` | Whitespace (space, tab, newline) |
| `\D`, `\W`, `\S` | Negated versions |
| `^` | Start of line |
| `$` | End of line |
| `\b` | Word boundary |
| `*` | 0 or more |
| `+` | 1 or more |
| `?` | 0 or 1 (optional) |
| `{n}` | Exactly n |
| `{n,m}` | Between n and m |
| `[abc]` | Character class |
| `[^abc]` | Negated character class |
| `(...)` | Capture group |
| `(?:...)` | Non-capturing group |
| `a\|b` | Alternation |

## Character Classes

### Shorthand Classes

| Class | Equivalent | Matches |
|-------|-----------|---------|
| `\d` | `[0-9]` | Digit |
| `\D` | `[^0-9]` | Non-digit |
| `\w` | `[a-zA-Z0-9_]` | Word character |
| `\W` | `[^a-zA-Z0-9_]` | Non-word character |
| `\s` | `[ \t\n\r\f\v]` | Whitespace |
| `\S` | `[^ \t\n\r\f\v]` | Non-whitespace |
| `\h` | `[ \t]` | Horizontal whitespace (PCRE) |
| `\v` | `[\n\r\f\v]` | Vertical whitespace (PCRE) |

### POSIX Classes (use inside `[...]`)

| Class | Matches |
|-------|---------|
| `[:alpha:]` | Letters |
| `[:digit:]` | Digits |
| `[:alnum:]` | Letters and digits |
| `[:upper:]` | Uppercase letters |
| `[:lower:]` | Lowercase letters |
| `[:space:]` | Whitespace |
| `[:punct:]` | Punctuation |
| `[:print:]` | Printable characters |
| `[:graph:]` | Visible characters (no space) |
| `[:xdigit:]` | Hex digits [0-9a-fA-F] |

Usage: `[[:alpha:]]` (double brackets when inside a character class).

### Custom Character Classes

```
[aeiou]          # Vowels
[a-z]            # Lowercase letters
[A-Z]            # Uppercase letters
[0-9]            # Digits
[a-zA-Z0-9]     # Alphanumeric
[^aeiou]         # NOT vowels
[a-z&&[^m-r]]   # a-z except m-r (intersection, Java/Rust)
[-.]             # Literal hyphen and dot (hyphen first or last)
[\\^]            # Literal backslash and caret
```

## Quantifiers

### Greedy (match as much as possible)

| Quantifier | Meaning |
|-----------|---------|
| `*` | 0 or more |
| `+` | 1 or more |
| `?` | 0 or 1 |
| `{3}` | Exactly 3 |
| `{2,5}` | 2 to 5 |
| `{2,}` | 2 or more |

### Lazy (match as little as possible)

Add `?` after the quantifier:

| Lazy | Meaning |
|------|---------|
| `*?` | 0 or more (lazy) |
| `+?` | 1 or more (lazy) |
| `??` | 0 or 1 (lazy) |
| `{2,5}?` | 2 to 5 (lazy) |

Example difference:
```
Input:   <b>bold</b> and <b>more</b>
Greedy:  <b>.*</b>     matches "<b>bold</b> and <b>more</b>"
Lazy:    <b>.*?</b>    matches "<b>bold</b>" then "<b>more</b>"
```

### Possessive (no backtracking, PCRE/Java)

Add `+` after the quantifier: `*+`, `++`, `?+`

Possessive quantifiers never give back matched characters, which prevents catastrophic backtracking but may cause the match to fail where greedy would succeed.

## Anchors

| Anchor | Matches |
|--------|---------|
| `^` | Start of line (or string with `\A`) |
| `$` | End of line (or string with `\z` / `\Z`) |
| `\b` | Word boundary |
| `\B` | Non-word boundary |
| `\A` | Start of string (never affected by multiline flag) |
| `\z` | End of string (absolute) |
| `\Z` | End of string (allows trailing newline) |

```
\bword\b     # "word" as a whole word
^line$       # Entire line equals "line"
\Afirst      # "first" at the very start of the string
```

## Groups

### Capturing Groups

```
(abc)            # Capture group 1
(a)(b)(c)        # Groups 1, 2, 3
(a(b)c)          # Nested: group 1 = "abc", group 2 = "b"
```

### Named Groups

```
(?P<name>pattern)    # Python, PCRE (also (?<name>pattern) in PCRE2)
(?<name>pattern)     # JavaScript, .NET, Rust, Java, PCRE2
```

### Non-Capturing Groups

```
(?:abc)              # Group without capturing
```

### Backreferences

```
(foo)\1              # Match "foo" followed by "foo" again
(?P<word>\w+)\s+\k<word>   # Named backreference (PCRE)
\1, \2, \3           # Numbered backreferences
```

## Lookaround Assertions

Lookaround matches a position without consuming characters (zero-width).

### Lookahead

```
foo(?=bar)       # "foo" followed by "bar" (positive lookahead)
foo(?!bar)       # "foo" NOT followed by "bar" (negative lookahead)
```

### Lookbehind

```
(?<=foo)bar      # "bar" preceded by "foo" (positive lookbehind)
(?<!foo)bar      # "bar" NOT preceded by "foo" (negative lookbehind)
```

**Lookbehind restrictions**: in most flavors (PCRE, Python, JavaScript), lookbehind must have a fixed length. Variable-length lookbehind is supported in .NET, Java 13+, and Python's `regex` module.

### Practical Examples

```
# Password: at least one digit, one uppercase, one lowercase
^(?=.*\d)(?=.*[a-z])(?=.*[A-Z]).{8,}$

# Number NOT followed by "px"
\d+(?!px)

# Dollar amount
(?<=\$)\d+(\.\d{2})?

# Word between quotes
(?<=")([^"]+)(?=")
```

## Alternation

```
cat|dog              # "cat" or "dog"
(cat|dog) food       # "cat food" or "dog food"
colou?r              # "color" or "colour"
gr[ae]y              # "gray" or "grey"
```

## Flags / Modifiers

| Flag | Name | Effect |
|------|------|--------|
| `i` | Case insensitive | `a` matches `A` |
| `m` | Multiline | `^` and `$` match line boundaries |
| `s` | Dotall / single-line | `.` matches newline |
| `x` | Extended / verbose | Ignore whitespace and `#` comments |
| `g` | Global | Match all occurrences (not all flavors) |
| `u` | Unicode | Full Unicode matching |

Inline flags (within pattern):
```
(?i)pattern          # Case insensitive
(?im)pattern         # Case insensitive + multiline
(?i:pattern)         # Scoped to group only
```

## Common Patterns

### Email Address (simplified)

```
[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}
```

### URL

```
https?://[^\s/$.?#][^\s]*
```

### IPv4 Address

```
\b(?:\d{1,3}\.){3}\d{1,3}\b
```

Strict (0-255 per octet):
```
\b(?:(?:25[0-5]|2[0-4]\d|1\d{2}|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d{2}|[1-9]?\d)\b
```

### IPv6 Address (simplified)

```
(?:[0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}
```

### Date (YYYY-MM-DD)

```
\d{4}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01])
```

### Time (HH:MM:SS, 24h)

```
(?:[01]\d|2[0-3]):[0-5]\d:[0-5]\d
```

### Phone Number (US)

```
(?:\+1[-.\s]?)?(?:\(?\d{3}\)?[-.\s]?)?\d{3}[-.\s]?\d{4}
```

### Hex Color

```
#(?:[0-9a-fA-F]{3}){1,2}\b
```

### Semantic Version

```
\bv?(\d+)\.(\d+)\.(\d+)(?:-([\w.]+))?(?:\+([\w.]+))?\b
```

### UUID

```
[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}
```

### File Path (Unix)

```
(?:/[\w.-]+)+/?
```

### Whitespace Normalization

```
\s+         # Match consecutive whitespace (for replacement with single space)
^\s+|\s+$   # Leading or trailing whitespace (for trimming)
```

## Flavor Differences

| Feature | PCRE | JavaScript | Python | Rust | POSIX BRE | POSIX ERE |
|---------|------|------------|--------|------|-----------|-----------|
| `\d` | Yes | Yes | Yes | Yes | No | No |
| Named groups | `(?P<n>)` | `(?<n>)` | `(?P<n>)` | `(?P<n>)` | No | No |
| Lookbehind | Fixed | Fixed (ES2018) | Fixed | No | No | No |
| Backrefs | Yes | Yes | Yes | No | `\1` | No |
| Possessive `++` | Yes | No | No | No | No | No |
| Atomic `(?>)` | Yes | No | No | Yes | No | No |
| Unicode `\p{L}` | Yes | Yes (u flag) | Yes | Yes | No | No |
| `\b` | Yes | Yes | Yes | Yes | No | No |
| Multiline `.` | `(?s)` | `s` flag | `re.DOTALL` | `(?s)` | N/A | N/A |

### Key flavor notes

- **Rust (`regex` crate)**: no backreferences, no lookaround (use `fancy-regex` for those). Guaranteed linear time.
- **JavaScript**: lookbehind added in ES2018 (modern browsers). No possessive quantifiers or atomic groups.
- **Python**: `re` module has fixed-length lookbehind. `regex` module (third-party) supports variable-length lookbehind.
- **POSIX BRE**: requires escaping `(`, `)`, `{`, `}` — they are literal without backslash. Used by `grep` (default), `sed`.
- **POSIX ERE**: `(`, `)`, `{`, `}` are special without backslash. Used by `grep -E`, `awk`, `sed -E`.

## Testing Strategies

```bash
# Test with grep (POSIX BRE)
echo "test123" | grep '[0-9]\+'

# Test with grep -E (ERE) or grep -P (PCRE, GNU grep)
echo "test123" | grep -E '[0-9]+'
echo "test123" | grep -P '\d+'

# Test with sed
echo "2024-01-15" | sed -E 's/([0-9]{4})-([0-9]{2})-([0-9]{2})/\3\/\2\/\1/'

# Test with Python
python3 -c "import re; print(re.findall(r'\d+', 'abc123def456'))"

# Test with Rust
# In code: regex::Regex::new(r"\d+").unwrap().find("abc123")

# Online tools: regex101.com (supports PCRE, JS, Python, Go, Rust)
```

## Performance Tips

- Avoid catastrophic backtracking: `(a+)+b` on "aaaaaac" causes exponential time
- Use possessive quantifiers (`++`) or atomic groups (`(?>...)`) when available
- Anchored patterns (`^...$`) are faster than unanchored
- Character classes `[aeiou]` are faster than alternation `a|e|i|o|u`
- Avoid `.*` at the start of a pattern; use a more specific prefix
- Pre-compile regex objects when matching in a loop (all languages)
- Use non-capturing groups `(?:...)` when you do not need the captured text
- Rust's `regex` crate guarantees O(n) matching; if you need backreferences, use `fancy-regex` but be aware of potential exponential time

## Important Notes

- Always use raw strings in programming languages to avoid double-escaping: Python `r"\d+"`, Rust `r"\d+"`
- `\b` matches a position between a word character and a non-word character, not a character itself
- `.` does not match `\n` by default; enable dotall/single-line mode (`s` flag) if needed
- `^` and `$` match start/end of string by default; enable multiline mode (`m` flag) for line boundaries
- In character classes, most special characters lose their meaning: `[.]` matches a literal dot
- Hyphen in character class must be first, last, or escaped: `[a-z]` is a range, `[-az]` includes literal `-`
- Caret `^` negates a character class only when it is the first character: `[^abc]`
- Always test regex against edge cases: empty strings, very long inputs, Unicode, special characters
