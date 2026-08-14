# WinDivert 2.2.2

ARC Live dynamically loads the official x64 WinDivert 2.2.2 release in passive
`WINDIVERT_FLAG_SNIFF | WINDIVERT_FLAG_RECV_ONLY` mode.

- Upstream: https://github.com/basil00/WinDivert/tree/v2.2.2
- Binary release SHA-256:
  `63CB41763BB4B20F600B6DE04E991A9C2BE73279E317D4D82F237B150C5F3F15`
- Source archive SHA-256:
  `65EC79C9E6AFA99F648A3F4D1F6DB794640B40D0B65BD438770EA503EE14ECB7`
- License: LGPL-3.0-or-later or GPL-2.0; ARC Live uses the LGPL-3.0 option.

The DLL and driver are not statically linked and can be replaced with a
compatible build by placing `WinDivert.dll` and `WinDivert64.sys` beside
`arc-live.exe`.
