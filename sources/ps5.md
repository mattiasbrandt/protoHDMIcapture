# PlayStation 5

Suggest: RES=1920x1080 FPS=60
Status: tested 2026-09-06
Wake: press the PS button on the controller

- HDCP: Off (Settings › System › HDMI › Enable HDCP). Capture shows black until it is off; Netflix and other video apps need it back on.
- Audio Format (Priority): Linear PCM (Settings › Sound › Audio Output). Bitstream (Dolby, DTS) gives silence or noise.
- HDR: Off (Settings › Screen and Video › Video Output › HDR). The card captures SDR; HDR comes out washed out.
- Resolution: 1080p (Settings › Screen and Video › Video Output › Resolution). "4K" on the box is the loop-out, not the capture.
- 120 Hz Output: Off (Settings › Screen and Video › Video Output).
- VRR: Off (Settings › Screen and Video › Video Output).
- RGB Range: Automatic (Settings › Screen and Video › Video Output). If blacks look crushed or grey, set LEVELS on the PC side instead.

## References

- [PlayStation: How to change PS5 console resolution](https://www.playstation.com/en-us/support/hardware/ps5-4k-resolution-guide/): Video Output paths and setting names (Sony)
- [PowerPyx: PS5 how to disable HDCP](https://www.powerpyx.com/ps5-how-to-disable-hdcp/): the HDCP path (third party)
- [Elgato: HDCP and Elgato Game Capture devices](https://help.elgato.com/hc/en-us/articles/360040482032): why capture needs HDCP off
- The Audio Format path is carried over from the earlier README; no Sony page for it was found.
