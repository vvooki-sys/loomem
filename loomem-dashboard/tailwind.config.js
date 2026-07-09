/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        // Loomem DS v2 semantic tokens. Values mirror the CSS vars in
        // src/index.css :root (duplication is deliberate — pointing Tailwind
        // at var() would break opacity modifiers).
        surface: {
          bg: '#FBF8F1',
          panel: '#FFFFFF',
          hover: '#F4EFE4',
          selected: '#EEF6FD',
        },
        line: {
          DEFAULT: '#DED7C8',
          strong: '#B7AE9E',
        },
        ink: {
          DEFAULT: '#1F1B16',
          muted: '#6B6256',
          subtle: '#8E8474',
        },
        brand: {
          DEFAULT: '#1684DC',
          hover: '#0F69B8',
        },
        danger: {
          DEFAULT: '#D2553B',
          bg: '#FBE9E3',
        },
        success: '#2E9E6B',
        warn: {
          DEFAULT: '#CE7D08',
          bg: '#FEF6E7',
        },
      },
      fontFamily: {
        display: ['Fraunces Variable', 'Fraunces', 'Iowan Old Style', 'Georgia', 'serif'],
      },
      animation: {
        'pulse-slow': 'pulse 3s cubic-bezier(0.4, 0, 0.6, 1) infinite',
        'fade-in': 'fadeIn 0.3s ease-in-out',
      },
      keyframes: {
        fadeIn: {
          '0%': { opacity: '0', transform: 'translateY(4px)' },
          '100%': { opacity: '1', transform: 'translateY(0)' },
        },
      },
    },
  },
  plugins: [],
};
