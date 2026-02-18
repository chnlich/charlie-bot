import type { Config } from 'tailwindcss'

export default {
  content: ['./index.html', './src/**/*.{js,ts,jsx,tsx}'],
  theme: {
    extend: {
      colors: {
        sidebar: '#1a1d21',
        surface: '#222529',
        border: '#3a3d43',
      },
    },
  },
  plugins: [],
} satisfies Config
