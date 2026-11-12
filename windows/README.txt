Tightwire for Windows
=====================
1. Double-click "Install Tightwire.cmd" and approve the administrator prompt
   (it adds one firewall rule, for Private and Domain networks only: if
   Windows calls your home network Public, set it to Private in Settings >
   Network). It copies Tightwire to
   %LOCALAPPDATA%\Programs\Tightwire, offers to install usbip-win2 (only
   needed for Stream Deck handoff), adds a Start menu entry and starts the
   tray app.
   tightwire.exe is not code-signed. If Windows SmartScreen says "Windows
   protected your PC" (it can when the files came through a browser, email
   or chat), choose "More info" > "Run anyway" only for a build you made
   yourself from https://github.com/thesammykins/tightwire.
2. Right-click the Tightwire icon in the notification area > Pair..., then on
   the Mac choose "Pair with a PC..." and confirm both show the same code.
3. Output device: tray menu > Output device. Pick a dedicated virtual endpoint
   (a Wave Link channel, "... (Elgato Virtual Audio)", or VB-Cable "CABLE Input").
   Nothing is played until you choose; the system default is never used.
Build the Mac and this PC from the same version of the source.
Stream Deck buttons: run  "%LOCALAPPDATA%\Programs\Tightwire\tightwire.exe" deck pc
(or deck mac / deck toggle).
Problems: tray menu > Open log, or run  tightwire.exe doctor  in a terminal.

Tightwire is free software under the GNU GPL v3 or later (LICENSE.txt).
Source code: https://github.com/thesammykins/tightwire
Third-party licences: THIRD-PARTY-NOTICES.txt.
Not affiliated with Elgato or Corsair.
