<#
Pantalla completa con imagen que cambia en cada cuadro, para que la grabadora tenga trabajo como con un juego (con el
escritorio quieto, las grabadoras se saltean casi todo). La abre versus.ps1; sola: -Segundos 60. Se cierra con Esc.
#>
param([int]$Segundos = 60)

Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Windows.Forms;

public class Escena : Form {
    readonly System.Windows.Forms.Timer timer = new System.Windows.Forms.Timer();
    readonly DateTime end;
    readonly Font font = new Font("Consolas", 64, FontStyle.Bold);
    int frame;

    public Escena(int seconds) {
        FormBorderStyle = FormBorderStyle.None;
        StartPosition = FormStartPosition.Manual;
        Bounds = Screen.PrimaryScreen.Bounds;
        TopMost = true;
        ShowInTaskbar = false;
        DoubleBuffered = true;
        BackColor = Color.Black;
        end = DateTime.Now.AddSeconds(seconds);
        KeyDown += (s, e) => { if (e.KeyCode == Keys.Escape) Close(); };
        timer.Interval = 15;
        timer.Tick += (s, e) => { frame++; if (DateTime.Now > end) Close(); else Invalidate(); };
        timer.Start();
    }

    protected override void OnPaint(PaintEventArgs e) {
        Graphics g = e.Graphics;
        int w = ClientSize.Width, h = ClientSize.Height, rows = 24, rowH = h / rows + 1;
        g.Clear(Color.FromArgb(frame % 256, (frame * 3) % 256, 96));
        for (int i = 0; i < rows; i++) {
            int x = (i * 97 + frame * (4 + i % 7)) % (w + 300) - 300;
            using (var b = new SolidBrush(Color.FromArgb((i * 40 + frame) % 256, (i * 90) % 256, (255 - frame) & 255)))
                g.FillRectangle(b, x, i * rowH, 300, rowH);
        }
        g.DrawString(frame.ToString(), font, Brushes.White, w / 2 - 150, h / 2 - 50);
    }
}
'@

[System.Windows.Forms.Application]::Run((New-Object Escena $Segundos))
