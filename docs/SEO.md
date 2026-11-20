# OmniEngine Search and Indexing

The public Execution Studio root page describes OmniEngine's local-first Rust
runtime, managed API gateway, and current sandboxed language support. It is the
only public HTML page in the dashboard; API and execution routes are not
landing pages.

## Search crawler routes

The UI bridge serves these public resources:

| Route | Purpose |
| --- | --- |
| `/` | Execution Studio and product overview |
| `/robots.txt` | Allows public pages and excludes `/api/` and `/v1/` from crawling |
| `/sitemap.xml` | Sitemap containing the canonical Studio URL |
| `/googlebf0032297d3c5fef.html` | Google Search Console HTML-file ownership verification |

The verification route is intentionally served without authentication or Host
header allowlisting. This exception applies only to the exact verification
file; it does not change host validation, API authentication, or sandbox rules
for any other route.

The bridge emits canonical, Open Graph, and Twitter URLs for the public site.
Set `OMNI_PUBLIC_SITE_URL` to the HTTPS origin used by Search Console, such as
`https://omniengine.onrender.com`, when deploying behind a reverse proxy or
custom domain. It must be an origin without a path. If unset, the bridge uses
Render's `RENDER_EXTERNAL_HOSTNAME`, then a validated non-local request host,
and otherwise defaults to `https://omniengine.onrender.com`.

The sitemap lists only the public Studio root. `/api/` and `/v1/` paths are
disallowed in `robots.txt` to avoid treating operational endpoints as search
pages. This is a crawler hint, not an access-control mechanism; authentication
and sandbox enforcement remain in the gateway.

## Google Search Console verification

1. Deploy the site so `https://omniengine.onrender.com/` serves the UI bridge.
2. In Google Search Console, add the `https://omniengine.onrender.com/` URL
   prefix property.
3. Choose **HTML file** verification and place the supplied file
   `googlebf0032297d3c5fef.html` in `ui/` without changing its verification
   line.
4. Deploy the change, then open
   `https://omniengine.onrender.com/googlebf0032297d3c5fef.html` and confirm
   the response is `200 OK` with `text/html`.
5. Return to Search Console and click **VERIFY**. After ownership is
   confirmed, submit `https://omniengine.onrender.com/sitemap.xml`.

Verification confirms site ownership; it does not guarantee indexing or a
ranking. Crawling and indexing happen gradually and remain subject to Google's
own policies and systems.

## Local checks

From the repository root, start the UI bridge on an unused port with a public
site origin configured:

```powershell
$env:PORT = '3000'
$env:OMNI_PUBLIC_SITE_URL = 'https://omniengine.onrender.com'
node .\ui\server.js
```

In another PowerShell window, verify the public resources:

```powershell
Invoke-WebRequest http://127.0.0.1:3000/googlebf0032297d3c5fef.html -UseBasicParsing
Invoke-WebRequest http://127.0.0.1:3000/robots.txt -UseBasicParsing
Invoke-WebRequest http://127.0.0.1:3000/sitemap.xml -UseBasicParsing
```
