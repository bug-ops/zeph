---
name: web-scrape
description: Extract structured data from web pages using CSS selectors. Use when the user asks to scrape a website, extract text or links from a page, or parse HTML content from a URL.
---
# Web Scrape

Extract data from web pages by specifying URL and CSS selectors.

## Extract page title
```scrape
{"url": "https://example.com", "select": "title", "extract": "text"}
```

## Extract all links
```scrape
{"url": "https://example.com", "select": "a", "extract": "attr:href", "limit": 20}
```

## Extract article text
```scrape
{"url": "https://example.com/article", "select": "article p", "extract": "text"}
```

## Extract table data
```scrape
{"url": "https://example.com/data", "select": "table tr td", "extract": "text", "limit": 50}
```

Always use HTTPS URLs. The selector follows CSS selector syntax.
Extract modes: "text" (text content), "html" (inner HTML), "attr:NAME" (attribute value).
