# Vendored JavaScript

Third-party code used by the web client, as single files without npm.
CI checks the files against `SHA256SUMS`.

| File | Source | License |
| --- | --- | --- |
| `livekit-client-2.22.4.umd.js` | `dist/livekit-client.umd.js` from the npm package [livekit-client 2.22.4](https://www.npmjs.com/package/livekit-client/v/2.22.4) (tarball integrity `sha512-UJ5K8gqP8uIO+tjhGeF+s6SzgLfxZKPxQjSqzzkF298QNYMnhYmXGQRMMh3GnsfaeswnsHHMSogMeb7SocH2zw==`) | Apache-2.0, see `livekit-client-LICENSE` |

To update: download the tarball from the npm registry, check its integrity
against the registry metadata, copy `dist/livekit-client.umd.js` under a
versioned name, update the `<script>` tag in `index.html` and regenerate
`SHA256SUMS` with `sha256sum`.
