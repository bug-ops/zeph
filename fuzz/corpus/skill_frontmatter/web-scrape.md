---
name: web-scrape
category: web
description: >
  Extract structured data from web pages using CSS selectors. Use when the user
  asks to scrape a website, extract text or links from a page, parse HTML
  content, get data from a table, read article content, collect image URLs,
  or pull structured information from any web page. Keywords: scrape, extract,
  parse, crawl, get page content, read website, pull data, HTML.
license: MIT
compatibility: Requires the scrape tool executor (built-in)
metadata:
  author: zeph
  version: "1.0"
---

# Web Page Data Extraction

Extract structured data from web pages by specifying a URL, a CSS selector to target elements, and an extraction mode.

## Command Format

````scrape
{"url": "URL", "select": "CSS_SELECTOR", "extract": "MODE", "limit": N}
````

### Parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `url` | Yes | Full URL of the page (use HTTPS) |
| `select` | Yes | CSS selector to target elements |
| `extract` | No | Extraction mode: `text` (default), `html`, or `attr:NAME` |
| `limit` | No | Maximum number of elements to return |

### Extract Modes

| Mode | Description | Example Output |
|------|-------------|----------------|
| `text` | Inner text content, stripped of HTML tags | `Hello World` |
| `html` | Inner HTML markup | `<strong>Hello</strong> World` |
| `attr:href` | Value of the specified attribute | `https://example.com` |
| `attr:src` | Value of the `src` attribute | `/images/logo.png` |
| `attr:class` | Value of the `class` attribute | `btn btn-primary` |
| `attr:id` | Value of the `id` attribute | `main-content` |
| `attr:data-*` | Value of any data attribute | `42` |

## CSS Selector Reference

### Basic Selectors

| Selector | Description | Example |
|----------|-------------|---------|
| `element` | All elements of that type | `p`, `div`, `h1`, `table` |
| `.class` | Elements with a specific class | `.article`, `.btn-primary` |
| `#id` | Element with a specific ID | `#content`, `#main` |
| `*` | All elements | `*` |
| `element.class` | Element with a specific class | `div.container`, `p.intro` |
| `element#id` | Element with a specific ID | `div#header` |

### Attribute Selectors

| Selector | Description | Example |
|----------|-------------|---------|
| `[attr]` | Has the attribute | `[href]`, `[data-id]` |
| `[attr=val]` | Attribute equals value exactly | `[type="text"]`, `[lang="en"]` |
| `[attr~=val]` | Attribute contains word (space-separated) | `[class~="active"]` |
| `[attr\|=val]` | Attribute starts with value or value followed by `-` | `[lang\|="en"]` |
| `[attr^=val]` | Attribute starts with value | `[href^="https"]` |
| `[attr$=val]` | Attribute ends with value | `[href$=".pdf"]` |
| `[attr*=val]` | Attribute contains value anywhere | `[href*="example"]` |

### Combinators

| Selector | Description | Example |
|----------|-------------|---------|
| `A B` | B is a descendant of A (any depth) | `article p` |
| `A > B` | B is a direct child of A | `ul > li` |
| `A + B` | B is the immediate next sibling of A | `h2 + p` |
| `A ~ B` | B is any subsequent sibling of A | `h2 ~ p` |
| `A, B` | Either A or B (selector list) | `h1, h2, h3` |

### Pseudo-Classes

| Selector | Description | Example |
|----------|-------------|---------|
| `:first-child` | First child of its parent | `li:first-child` |
| `:last-child` | Last child of its parent | `li:last-child` |
| `:nth-child(n)` | Nth child (1-based) | `tr:nth-child(2)` |
| `:nth-child(odd)` | Odd-numbered children | `tr:nth-child(odd)` |
| `:nth-child(even)` | Even-numbered children | `tr:nth-child(even)` |
| `:nth-of-type(n)` | Nth element of its type | `p:nth-of-type(3)` |
| `:first-of-type` | First element of its type | `p:first-of-type` |
| `:last-of-type` | Last element of its type | `p:last-of-type` |
| `:not(sel)` | Elements that do not match | `p:not(.ad)` |
| `:empty` | Elements with no children | `td:empty` |

## Common Extraction Patterns

### Page Title

```scrape
{"url": "https://example.com", "select": "title", "extract": "text"}
```

### Meta Description

```scrape
{"url": "https://example.com", "select": "meta[name='description']", "extract": "attr:content"}
```

### Open Graph Tags

```scrape
{"url": "https://example.com", "select": "meta[property^='og:']", "extract": "attr:content"}
```

### All Links on a Page

```scrape
{"url": "https://example.com", "select": "a[href]", "extract": "attr:href", "limit": 50}
```

### External Links Only

```scrape
{"url": "https://example.com", "select": "a[href^='http']", "extract": "attr:href", "limit": 30}
```

### All Images

```scrape
{"url": "https://example.com", "select": "img", "extract": "attr:src", "limit": 20}
```

### Images with Alt Text

```scrape
{"url": "https://example.com", "select": "img[alt]", "extract": "attr:alt", "limit": 20}
```

### Article / Main Content

```scrape
{"url": "https://example.com/post", "select": "article p", "extract": "text"}
```

```scrape
{"url": "https://example.com/post", "select": "main p", "extract": "text"}
```

```scrape
{"url": "https://example.com/post", "select": ".content p, .post-body p", "extract": "text"}
```

### Headings (Document Structure)

```scrape
{"url": "https://example.com/post", "select": "h1, h2, h3", "extract": "text"}
```

### Navigation Links

```scrape
{"url": "https://example.com", "select": "nav a", "extract": "text", "limit": 20}
```

### Table Data

```scrape
{"url": "https://example.com/data", "select": "table thead th", "extract": "text"}
```

```scrape
{"url": "https://example.com/data", "select": "table tbody td", "extract": "text", "limit": 100}
```

```scrape
{"url": "https://example.com/data", "select": "table tbody tr", "extract": "text", "limit": 50}
```

### Ordered / Unordered Lists

```scrape
{"url": "https://example.com", "select": "ul.features li", "extract": "text"}
```

```scrape
{"url": "https://example.com", "select": "ol li", "extract": "text"}
```

### Definition Lists

```scrape
{"url": "https://example.com", "select": "dl dt", "extract": "text"}
```

```scrape
{"url": "https://example.com", "select": "dl dd", "extract": "text"}
```

### Form Fields

```scrape
{"url": "https://example.com/form", "select": "input[type='text'], input[type='email'], textarea", "extract": "attr:name"}
```

### Code Blocks

```scrape
{"url": "https://example.com/docs", "select": "pre code", "extract": "text", "limit": 10}
```

### Specific Data Attributes

```scrape
{"url": "https://example.com", "select": "[data-price]", "extract": "attr:data-price"}
```

## Multi-Step Extraction Strategy

For complex pages, extract data in multiple steps:

1. **Discover structure** — get headings and landmark elements first:
   ```scrape
   {"url": "https://example.com", "select": "h1, h2, h3, nav, main, article, section", "extract": "text", "limit": 30}
   ```

2. **Narrow down** — once you know the page structure, target the specific container:
   ```scrape
   {"url": "https://example.com", "select": "#results .item .title", "extract": "text"}
   ```

3. **Extract details** — get attributes or nested content from the targeted elements:
   ```scrape
   {"url": "https://example.com", "select": "#results .item a", "extract": "attr:href"}
   ```

## Selector Construction Tips

### Finding the Right Selector

- Start broad (`p`, `div`, `a`) and narrow down based on results
- Use class names when available — they are the most reliable selectors
- Combine element + class for precision: `div.product-card` instead of `.product-card`
- Use attribute selectors for pages that rely on data attributes: `[data-testid="price"]`
- Chain selectors for nested content: `div.product-card > h3 > a`

### Common Page Structures

| Content Type | Likely Selectors |
|-------------|-----------------|
| Article body | `article p`, `main p`, `.content p`, `.post-body p` |
| Blog post title | `h1`, `article h1`, `.post-title` |
| Product name | `.product-name`, `.product-title`, `h1.title` |
| Product price | `.price`, `.product-price`, `[data-price]`, `span.amount` |
| Search results | `.result`, `.search-result`, `.item` |
| Navigation | `nav a`, `.nav-link`, `.menu a` |
| Sidebar | `aside`, `.sidebar`, `#sidebar` |
| Footer | `footer`, `.footer`, `#footer` |
| Breadcrumbs | `.breadcrumb a`, `nav[aria-label="breadcrumb"] a` |

## Pagination

To scrape multiple pages, modify the URL for each page:

```scrape
{"url": "https://example.com/items?page=1", "select": ".item-title", "extract": "text", "limit": 25}
```

```scrape
{"url": "https://example.com/items?page=2", "select": ".item-title", "extract": "text", "limit": 25}
```

Look for pagination patterns in the page:
```scrape
{"url": "https://example.com/items", "select": ".pagination a", "extract": "attr:href"}
```

## Important Notes

- Always use HTTPS URLs when possible
- Set a reasonable `limit` to avoid overwhelming output — start with 10-20 and increase if needed
- If a selector returns no results, the page may use JavaScript rendering (SPA). Try broader selectors or a different approach
- Some sites block automated access — if a request fails, the site may require authentication or have anti-scraping measures
- Respect `robots.txt` and site terms of service
- For tables, extract headers (`thead th`) and data (`tbody td`) separately for cleaner results
- When extracting links with `attr:href`, relative URLs (starting with `/`) need the base domain prepended
- Use `extract: "html"` when you need to preserve formatting (bold, links, lists) within elements
- If the initial selector is too broad and returns noise, add parent context: instead of `p`, try `article p` or `main p`
- For sites with dynamic class names (React/Next.js), use attribute selectors: `[data-testid="..."]`, `[role="..."]`, `[aria-label="..."]`
