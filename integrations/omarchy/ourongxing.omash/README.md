# omash Omarchy bar widget

This Quickshell plugin adds an omash icon to the Omarchy bar. Its compact,
two-column panel exposes Mihomo's Rule, Global, and Direct modes plus every
selector proxy group. Groups retain their `runtime.yaml` declaration order;
the selected group's proxies retain Mihomo's order. The panel also shows the
current proxy and can test latency for every proxy in the active group.

The `omash` binary must be available on `PATH`. Install the plugin from the
repository root with:

```bash
mkdir -p ~/.config/omarchy/plugins
cp -r integrations/omarchy/ourongxing.omash ~/.config/omarchy/plugins/
omarchy plugin enable ourongxing.omash --section right
```
