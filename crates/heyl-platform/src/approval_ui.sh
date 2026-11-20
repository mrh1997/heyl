# heyl's approval window on Linux: zenity, or kdialog on KDE (DESIGN.md §5).
#
# Run by heyl as `sh -c <this> heyl-ui <unlock|pair>`. It speaks the same
# protocol as a `--ui-cmd`: heyl ends stdin when the wait is over, and this
# script prints `cancel` if the person dismisses the dialog. It also prints
# `shown` once a dialog is really up, which is what tells heyl it need not
# draw the pairing code in the terminal as well.
#
# Exit 3 means "no dialog could be shown": heyl falls back to the terminal.

kind=$1
[ -n "$WAYLAND_DISPLAY$DISPLAY" ] || exit 3

# Kept on fd 3: a background job's stdin is /dev/null in a non-interactive
# shell, so the watcher below could not read heyl's end of stdin otherwise.
exec 3<&0

if [ "$kind" = pair ]; then
    title="heyl: pair with your phone"
else
    title="heyl: approve on your phone"
fi

# The code is drawn black on white whatever the desktop theme: the other
# polarity looks right and does not scan.
if command -v zenity >/dev/null 2>&1; then
    if [ "$kind" = pair ]; then
        zenity --info --title="$title" --ok-label=Cancel --no-wrap --text="Scan this code with the heylogin app:

<span font_family=\"monospace\" foreground=\"black\" background=\"white\">$HEYL_UI_QR</span>
<small>$HEYL_UI_URL</small>" >/dev/null 2>&1 &
    else
        zenity --info --title="$title" --ok-label=Cancel --no-wrap --no-markup \
            --text="$HEYL_UI_HEADING

$HEYL_UI_MESSAGE" >/dev/null 2>&1 &
    fi
elif command -v kdialog >/dev/null 2>&1; then
    if [ "$kind" = pair ]; then
        kdialog --title "$title" --ok-label Cancel --msgbox "<html>Scan this code with the heylogin app:<pre style=\"color:black;background:white;line-height:100%\">$HEYL_UI_QR</pre><small>$HEYL_UI_URL</small></html>" >/dev/null 2>&1 &
    else
        kdialog --title "$title" --ok-label Cancel --msgbox "$HEYL_UI_HEADING

$HEYL_UI_MESSAGE" >/dev/null 2>&1 &
    fi
else
    exit 3
fi
dialog=$!

# A dialog that cannot reach the display dies at once. One that is still
# there a second later is on screen.
sleep 1
kill -0 "$dialog" 2>/dev/null || exit 3
echo shown

# The end of stdin is heyl saying "done": take the dialog down.
(
    cat <&3 >/dev/null
    kill "$dialog" 2>/dev/null
) &

# Silenced: the shell's own "Terminated" notice would land on heyl's stderr.
wait "$dialog" 2>/dev/null
status=$?
# Killed (128 + signal) means heyl closed it. Any other end is the person's:
# the Cancel button, or the window's close box.
[ "$status" -gt 128 ] || echo cancel
