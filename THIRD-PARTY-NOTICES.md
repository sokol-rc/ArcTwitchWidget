# Third-party notices

ARC Live uses `pcapsql-core` 0.3.1 by Max Tottenham under the MIT License.
The complete license text is retained at `vendor/pcapsql-core/LICENSE`.

The vendored copy carries TLS-decryption and protocol-registration changes made
by RaidTheory LLC for ARCTracker Sync and published by them under the same MIT
terms; see `vendor/pcapsql-core/LOCAL-MODIFICATIONS.md`. ARC Live adds a lenient
SSLKEYLOGFILE reader for a file that may be actively written. The original
strict parser remains the default API.

ARC Live also dynamically loads WinDivert 2.2.2 by basil under the GNU Lesser
General Public License version 3. WinDivert remains a separately replaceable
DLL and driver; its complete license and corresponding source archive are
shipped with ARC Live distributions. Upstream source:
https://github.com/basil00/WinDivert/tree/v2.2.2
