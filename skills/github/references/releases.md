# Releases

## List

```bash
gh release list --limit 5
gh release list --limit 10 --json tagName,publishedAt,isPrerelease
```

## View

```bash
gh release view TAG
gh release view TAG --json tagName,body,assets
gh release view --latest
```

## Create

```bash
gh release create TAG --title "TITLE" --notes "NOTES"
gh release create TAG --generate-notes
gh release create TAG --generate-notes --prerelease
gh release create TAG --title "TITLE" --notes-file release-notes.md
gh release create TAG --target BRANCH --title "TITLE" --notes "NOTES"
gh release create TAG --discussion-category "Announcements" --generate-notes
```

## Upload assets

```bash
gh release create TAG ./dist/*.tar.gz --title "TITLE" --notes "NOTES"
gh release upload TAG ./dist/*.tar.gz
gh release upload TAG ./artifact.zip --clobber  # overwrite existing
```

## Download

```bash
gh release download TAG
gh release download TAG --pattern "*.tar.gz"
gh release download TAG --pattern "*.tar.gz" --dir ./downloads
gh release download --latest --pattern "*.zip"
```

## Edit and delete

```bash
gh release edit TAG --title "NEW TITLE"
gh release edit TAG --notes "UPDATED NOTES"
gh release edit TAG --draft=false  # publish draft
gh release edit TAG --prerelease=false
gh release delete TAG --yes
gh release delete TAG --yes --cleanup-tag  # also delete the git tag
```
