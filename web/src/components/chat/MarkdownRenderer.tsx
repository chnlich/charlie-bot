import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'

interface Props {
  content: string
}

export function MarkdownRenderer({ content }: Props) {
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      components={{
        code({ children, className, ...rest }) {
          const match = /language-(\w+)/.exec(className ?? '')
          const isBlock = String(children).includes('\n')
          if (isBlock) {
            return (
              <pre className="bg-slate-900 rounded p-3 overflow-x-auto text-sm my-2">
                <code className={match ? `language-${match[1]}` : ''} {...rest}>
                  {children}
                </code>
              </pre>
            )
          }
          return (
            <code className="bg-slate-800 rounded px-1 py-0.5 text-sm font-mono" {...rest}>
              {children}
            </code>
          )
        },
        p({ children }) {
          return <p className="mb-2 last:mb-0">{children}</p>
        },
        ul({ children }) {
          return <ul className="list-disc list-inside mb-2 space-y-1">{children}</ul>
        },
        ol({ children }) {
          return <ol className="list-decimal list-inside mb-2 space-y-1">{children}</ol>
        },
        h1({ children }) {
          return <h1 className="text-lg font-bold mb-2 mt-3">{children}</h1>
        },
        h2({ children }) {
          return <h2 className="text-base font-bold mb-2 mt-3">{children}</h2>
        },
        h3({ children }) {
          return <h3 className="text-sm font-bold mb-1 mt-2">{children}</h3>
        },
        blockquote({ children }) {
          return (
            <blockquote className="border-l-2 border-slate-600 pl-3 text-slate-400 italic">
              {children}
            </blockquote>
          )
        },
      }}
    >
      {content}
    </ReactMarkdown>
  )
}
