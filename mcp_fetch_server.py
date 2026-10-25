#!/usr/bin/env python3
import json
import sys
import urllib.request
import re
from html.parser import HTMLParser

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

class TextExtractor(HTMLParser):
    def __init__(self):
        super().__init__()
        self.text_parts = []
        self.skip = False
        self.skip_tags = {'script', 'style', 'noscript', 'svg', 'meta', 'link', 'head'}
        
    def handle_starttag(self, tag, attrs):
        tag = tag.lower()
        if tag in self.skip_tags:
            self.skip = True
        elif tag in {'p', 'div', 'br', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'li', 'tr'}:
            self.text_parts.append('\n')
        elif tag == 'td' or tag == 'th':
            self.text_parts.append(' | ')
    
    def handle_endtag(self, tag):
        tag = tag.lower()
        if tag in self.skip_tags:
            self.skip = False
        elif tag in {'p', 'div', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6'}:
            self.text_parts.append('\n')
        elif tag == 'li':
            self.text_parts.append('\n')
    
    def handle_data(self, data):
        if self.skip:
            return
        text = data.strip()
        if text:
            self.text_parts.append(text)
    
    def get_text(self):
        text = ' '.join(self.text_parts)
        # Clean up whitespace
        text = re.sub(r'\n\s*\n+', '\n\n', text)
        text = re.sub(r'[ \t]+', ' ', text)
        text = re.sub(r' +\n', '\n', text)
        text = re.sub(r'\n +', '\n', text)
        return text.strip()

def extract_title(html):
    m = re.search(r'<title[^>]*>(.*?)</title>', html, re.I | re.S)
    if m:
        title = re.sub(r'<[^>]+>', '', m.group(1))
        return re.sub(r'\s+', ' ', title).strip()
    return None

def extract_main_content(html):
    # Try to extract main/article content
    # Remove scripts and styles first
    html_clean = re.sub(r'<script[^>]*>.*?</script>', '', html, flags=re.I | re.S)
    html_clean = re.sub(r'<style[^>]*>.*?</style>', '', html_clean, flags=re.I | re.S)
    html_clean = re.sub(r'<!--.*?-->', '', html_clean, flags=re.I | re.S)
    
    # Try to find main content areas
    for pattern in [r'<main[^>]*>(.*?)</main>', r'<article[^>]*>(.*?)</article>', r'<div[^>]*id="content"[^>]*>(.*?)</div>', r'<div[^>]*class="content"[^>]*>(.*?)</div>']:
        m = re.search(pattern, html_clean, flags=re.I | re.S)
        if m and len(m.group(1)) > 200:
            html_clean = m.group(1)
            break
    
    extractor = TextExtractor()
    try:
        extractor.feed(html_clean)
        return extractor.get_text()
    except:
        # Fallback to simple tag removal
        text = re.sub(r'<[^>]+>', ' ', html_clean)
        text = re.sub(r'\s+', ' ', text)
        return text.strip()

for line in sys.stdin:
    try:
        req = json.loads(line)
        method = req.get("method")
        id_ = req.get("id")
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": id_, "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "fetch", "version": "1.0"}}})
        elif method == "notifications/initialized":
            pass
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": id_, "result": {"tools": [{"name": "fetch", "description": "Fetch a URL and return clean, readable text content", "inputSchema": {"type": "object", "properties": {"url": {"type": "string"}, "max_length": {"type": "integer"}}, "required": ["url"]}}]}})
        elif method == "tools/call":
            name = req.get("params", {}).get("name")
            args = req.get("params", {}).get("arguments", {})
            if name == "fetch":
                url = args.get("url", "")
                max_len = int(args.get("max_length", 6000))
                try:
                    req2 = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0 (compatible; Jarvis/1.0)", "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"})
                    with urllib.request.urlopen(req2, timeout=15) as resp:
                        content_type = resp.headers.get('Content-Type', '')
                        data = resp.read()
                        # Try to decode
                        try:
                            if 'charset=' in content_type:
                                charset = content_type.split('charset=')[-1].split(';')[0].strip()
                                html = data.decode(charset, errors='replace')
                            else:
                                html = data.decode('utf-8', errors='replace')
                        except:
                            html = data.decode('latin-1', errors='replace')
                    
                    title = extract_title(html)
                    text_content = extract_main_content(html)
                    
                    # Build clean output
                    out_parts = []
                    if title:
                        out_parts.append(f"Title: {title}")
                    out_parts.append(f"URL: {url}")
                    out_parts.append("")  # blank line
                    out_parts.append(text_content)
                    
                    out = "\n".join(out_parts)
                    
                    if len(out) > max_len:
                        out = out[:max_len] + "\n\n[Content truncated...]"
                    
                    send({"jsonrpc": "2.0", "id": id_, "result": {"content": [{"type": "text", "text": out}]}})
                except Exception as e:
                    send({"jsonrpc": "2.0", "id": id_, "result": {"content": [{"type": "text", "text": f"Error fetching {url}: {str(e)}"}], "isError": True}})
            else:
                send({"jsonrpc": "2.0", "id": id_, "error": {"code": -32601, "message": "not found"}})
        else:
            if id_ is not None:
                send({"jsonrpc": "2.0", "id": id_, "error": {"code": -32601, "message": "method not found"}})
    except Exception:
        pass
