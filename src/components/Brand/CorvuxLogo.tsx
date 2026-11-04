import { splitProps, type JSX } from "solid-js";
import { settingsStore } from "../../stores/settings";

type LogoVariant = "auto" | "dark" | "light";

interface CorvuxLogoProps extends Omit<JSX.ImgHTMLAttributes<HTMLImageElement>, "src"> {
  /** "dark" is the light graphite mark for dark UI surfaces. */
  variant?: LogoVariant;
}

export default function CorvuxLogo(props: CorvuxLogoProps) {
  const [local, imgProps] = splitProps(props, ["variant", "alt", "draggable"]);

  const src = () => {
    const variant = local.variant ?? "auto";
    if (variant === "dark") return "/corvux-logo-dark.png";
    if (variant === "light") return "/corvux-logo-light.png";
    // Parchment is a light-bg theme, so it needs the black mark — same as
    // the Light theme. Everything else (dark, nord, synthwave, corvid) is
    // a dark-bg theme and uses the white mark.
    const theme = settingsStore.theme();
    return theme === "light" || theme === "parchment"
      ? "/corvux-logo-light.png"
      : "/corvux-logo-dark.png";
  };

  return (
    <img
      {...imgProps}
      src={src()}
      alt={local.alt ?? "Corvux"}
      draggable={local.draggable ?? false}
    />
  );
}
