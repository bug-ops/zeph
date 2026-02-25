# Gists

## List

```bash
gh gist list --limit 10
gh gist list --public --limit 10
gh gist list --secret --limit 10
```

## View

```bash
gh gist view GIST_ID
gh gist view GIST_ID --filename FILE
gh gist view GIST_ID --raw
```

## Create

```bash
gh gist create FILE --public --desc "DESCRIPTION"
gh gist create FILE --desc "DESCRIPTION"  # secret by default
gh gist create FILE1 FILE2 --desc "DESCRIPTION"
echo "content" | gh gist create --public --desc "DESCRIPTION"
gh gist create FILE --web  # open in browser after creating
```

## Edit and delete

```bash
gh gist edit GIST_ID
gh gist edit GIST_ID --filename FILE --add NEW_FILE
gh gist rename GIST_ID OLD_NAME NEW_NAME
gh gist delete GIST_ID
```

## Clone

```bash
gh gist clone GIST_ID
```
