# Anna's Archive MCP

A Model Context Protocol (MCP) server for [Anna's Archive](https://annas-archive.org), providing access to search and retrieve information about books, papers, magazines, comics, and other documents.

## Installation

```bash
cargo install annas-archive-mcp
```

Or build from source:

```bash
git clone https://github.com/RemiKalbe/annas-archive-mcp
cd annas-archive-mcp
cargo install --path annas-archive-mcp
```

## Usage

### Claude Desktop

Add to your Claude Desktop configuration (`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS):

```json
{
  "mcpServers": {
    "annas-archive": {
      "command": "annas-archive-mcp",
      "env": {
        "PATH": "${HOME}/.cargo/bin:${PATH}",
        "ANNAS_ARCHIVE_API_KEY": "your-api-key"
      }
    }
  }
}
```

**Getting an API key:**
1. Create an account on [annas-archive.gd](https://annas-archive.gd)
2. Your API key is your "Secret key" (the key you use to log in)
3. Go to the [donate page](https://annas-archive.gd/donate) to become a member

## Browser check

Anna's Archive puts a browser check in front of search. Members whose tier includes skipping browser checks are let through with just the API key, within an hourly limit. Everyone else, and members over that limit, have to pass the check in a browser.

When a search is blocked, the `pass_browser_check` tool opens a Chrome window on a search page. Pass the check there and the window closes by itself; searches then work for about 15 minutes. This needs Chrome (or Chromium, Brave, Edge) on the machine running the server. Set `ANNAS_ARCHIVE_CHROME` to the browser binary if it is not found.

## Available Tools

| Tool | Description | Requires API Key |
|------|-------------|------------------|
| `search` | Search for books, papers, magazines, comics, and other documents | No |
| `pass_browser_check` | Open a browser window to pass the browser check when search is blocked | No |
| `get_details` | Get detailed metadata for an item by its MD5 hash | Yes |
| `get_download_url` | Get a fast download URL for an item | Yes |

## License

MIT
